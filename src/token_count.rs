//! Per-turn and per-position token counts derived from the durable message array.

use std::collections::{HashSet, VecDeque};
use std::ops::Range;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client;
use crate::config::ResolvedEndpoint;
use crate::session::{Message, Role, TurnTokenCache};
use crate::tool_call_id;

const DIGEST_DOMAIN: &[u8] = b"harness/turn-token-cache/v1\0";
const REASONING_FIELDS: [&str; 3] = ["reasoning", "reasoning_content", "reasoning_details"];

/// Everything needed to count one durable turn without retaining the whole session.
#[derive(Clone)]
pub struct Plan {
    pub range: Range<usize>,
    pub digest: String,
    prefixes: Vec<Prefix>,
}

#[derive(Clone)]
struct Prefix {
    messages: Vec<Value>,
    position: Option<usize>,
}

/// A cached result and the message range it describes in the current session.
#[derive(Clone)]
pub struct CountedTurn {
    pub range: Range<usize>,
    pub cache: TurnTokenCache,
}

/// A session-wide total retains whether any contributing turn is heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenTotal {
    pub count: u64,
    pub estimated: bool,
}

/// Incremental background tokenizer queue and the immediately available display values.
pub struct Counter {
    http: reqwest::Client,
    endpoint: ResolvedEndpoint,
    seen_revision: Option<u64>,
    seen_active_user: Option<Option<usize>>,
    plans: Vec<Plan>,
    turns: Vec<CountedTurn>,
    pending: VecDeque<Plan>,
    running: Option<(String, tokio::task::JoinHandle<TurnTokenCache>)>,
}

impl Counter {
    /// Start with no observed session state; the first frame performs reconciliation.
    pub fn new(http: reqwest::Client, endpoint: ResolvedEndpoint) -> Self {
        Self {
            http,
            endpoint,
            seen_revision: None,
            seen_active_user: None,
            plans: Vec::new(),
            turns: Vec::new(),
            pending: VecDeque::new(),
            running: None,
        }
    }

    /// Reconcile only after provider-visible messages change; cache-only saves do not rescan.
    pub fn refresh(
        &mut self,
        session: &mut crate::session::Session,
        active_user: Option<usize>,
    ) -> anyhow::Result<()> {
        if self.seen_revision == Some(session.revision())
            && self.seen_active_user == Some(active_user)
        {
            return Ok(());
        }
        self.plans = plans(&session.messages, &self.endpoint)?;
        let valid: HashSet<_> = self.plans.iter().map(|plan| plan.digest.as_str()).collect();
        let retained: Vec<_> = session
            .local
            .turn_token_cache
            .iter()
            .filter(|entry| valid.contains(entry.digest.as_str()))
            .cloned()
            .collect();
        if retained != session.local.turn_token_cache {
            session.save_token_cache(retained)?;
        }

        let running = self.running.as_ref().map(|(digest, _)| digest.as_str());
        self.pending.clear();
        self.turns.clear();
        for plan in &self.plans {
            let cached = session.local.turn_token_cache.iter().find(|entry| {
                entry.digest == plan.digest && entry.positions.len() == plan.position_count()
            });
            let cache = cached
                .cloned()
                .unwrap_or_else(|| cached_or_estimated(plan, &[]));
            self.turns.push(CountedTurn {
                range: plan.range.clone(),
                cache,
            });
            let incomplete = active_user.is_some_and(|index| plan.range.contains(&index));
            if cached.is_none() && !incomplete && running != Some(plan.digest.as_str()) {
                self.pending.push_back(plan.clone());
            }
        }
        self.seen_revision = Some(session.revision());
        self.seen_active_user = Some(active_user);
        Ok(())
    }

    /// Current ranges and counts used to build the compact overlay.
    pub fn turns(&self) -> &[CountedTurn] {
        &self.turns
    }

    /// Sum the same per-turn values shown in the context overlay.
    pub fn total(&self) -> TokenTotal {
        TokenTotal {
            count: self.turns.iter().map(|turn| turn.cache.total).sum(),
            estimated: self.turns.iter().any(|turn| turn.cache.estimated),
        }
    }

    /// Wait for the next sequential provider result, or forever when no work exists.
    pub async fn next(&mut self) -> Option<(String, TurnTokenCache)> {
        if self.running.is_none()
            && let Some(plan) = self.pending.pop_front()
        {
            let digest = plan.digest.clone();
            let http = self.http.clone();
            let endpoint = self.endpoint.clone();
            self.running = Some((
                digest,
                tokio::spawn(async move { calculate(&http, &endpoint, &plan).await }),
            ));
        }
        let Some((digest, task)) = self.running.as_mut() else {
            return std::future::pending().await;
        };
        let result = task.await.ok().map(|cache| (digest.clone(), cache));
        self.running = None;
        result
    }

    /// Persist a result only if its exact turn still exists; stale background work is discarded.
    pub fn finish(
        &mut self,
        session: &mut crate::session::Session,
        digest: &str,
        cache: TurnTokenCache,
    ) -> anyhow::Result<()> {
        if !self.plans.iter().any(|plan| plan.digest == digest) {
            return Ok(());
        }
        let mut entries = session.local.turn_token_cache.clone();
        if let Some(existing) = entries.iter_mut().find(|entry| entry.digest == digest) {
            *existing = cache;
        } else {
            entries.push(cache);
        }
        session.save_token_cache(entries)?;
        self.seen_revision = None;
        self.seen_active_user = None;
        Ok(())
    }
}

impl Drop for Counter {
    /// Do not leave a tokenizer request detached when the TUI exits.
    fn drop(&mut self) {
        if let Some((_, task)) = self.running.take() {
            task.abort();
        }
    }
}

/// Split durable messages into user turns and standalone marked file injections.
pub fn plans(messages: &[Message], endpoint: &ResolvedEndpoint) -> anyhow::Result<Vec<Plan>> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index < messages.len() {
        if is_injection_start(messages, index) {
            if let Some(first) = start.take() {
                ranges.push((first..index, false));
            }
            ranges.push((index..index + 2, true));
            index += 2;
            continue;
        }
        if messages[index].role == Role::User {
            if let Some(first) = start.replace(index) {
                ranges.push((first..index, false));
            }
        } else if start.is_none() {
            start = Some(index);
        }
        index += 1;
    }
    if let Some(first) = start {
        ranges.push((first..messages.len(), false));
    }

    ranges
        .into_iter()
        .filter(|(range, _)| !range.is_empty())
        .map(|(range, injection)| build_plan(messages, range, injection, endpoint))
        .collect()
}

/// Reuse an exact cache hit; otherwise provide an immediate deterministic estimate.
pub fn cached_or_estimated(plan: &Plan, cache: &[TurnTokenCache]) -> TurnTokenCache {
    cache
        .iter()
        .find(|entry| entry.digest == plan.digest && entry.positions.len() == plan.position_count())
        .cloned()
        .unwrap_or_else(|| estimated(plan))
}

/// Count every progressive prefix; one failed request makes the whole turn heuristic.
pub async fn calculate(
    http: &reqwest::Client,
    endpoint: &ResolvedEndpoint,
    plan: &Plan,
) -> TurnTokenCache {
    let mut counts = Vec::with_capacity(plan.prefixes.len());
    for prefix in &plan.prefixes {
        match client::tokenize(http, endpoint, &prefix.messages).await {
            Ok(count) => counts.push(count),
            Err(_) => return estimated(plan),
        }
    }
    from_prefix_counts(plan, counts, false)
}

impl Plan {
    /// Number of visible context positions whose deltas are stored.
    pub fn position_count(&self) -> usize {
        self.prefixes
            .iter()
            .filter(|prefix| prefix.position.is_some())
            .count()
    }
}

/// A marked injection is one dedicated assistant call followed by its result.
fn is_injection_start(messages: &[Message], index: usize) -> bool {
    let Some(message) = messages.get(index) else {
        return false;
    };
    let Some(call) = message.tool_calls.as_ref().and_then(|calls| calls.first()) else {
        return false;
    };
    message.role == Role::Assistant
        && message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| calls.len() == 1)
        && tool_call_id::decode(&call.id)
            .is_some_and(|metadata| metadata.kind == tool_call_id::INJECTED_FILE)
        && messages.get(index + 1).is_some_and(|result| {
            result.role == Role::Tool && result.tool_call_id.as_deref() == Some(call.id.as_str())
        })
}

/// Build progressive valid chat slices in the same order as compact display positions.
fn build_plan(
    messages: &[Message],
    range: Range<usize>,
    injection: bool,
    endpoint: &ResolvedEndpoint,
) -> anyhow::Result<Plan> {
    let slice = &messages[range.clone()];
    let mut current = Vec::new();
    if injection {
        current.push(json!({"role": "user", "content": ""}));
    }
    let mut prefixes = Vec::new();
    let mut consumed_results = HashSet::new();
    let mut position = 0;

    for (offset, message) in slice.iter().enumerate() {
        if consumed_results.contains(&offset) {
            continue;
        }
        match message.role {
            Role::User => {
                current.push(json!(message));
                push_prefix(&mut prefixes, &current, Some(&mut position));
            }
            Role::Assistant => {
                let calls = message.tool_calls.as_deref().unwrap_or_default();
                let synthetic_only = message.content.is_empty()
                    && message.extra.is_empty()
                    && !calls.is_empty()
                    && calls.iter().all(is_synthetic);
                if synthetic_only {
                    current.push(json!(message));
                    append_results(slice, calls, &mut current, &mut consumed_results);
                    push_prefix(&mut prefixes, &current, None);
                    continue;
                }

                let original = json!(message);
                let mut assistant = original.as_object().cloned().unwrap_or_default();
                let original_calls = assistant.remove("tool_calls");
                let reasoning: Vec<_> = REASONING_FIELDS
                    .into_iter()
                    .filter_map(|key| assistant.remove(key).map(|value| (key, value)))
                    .collect();
                let assistant_index = current.len();
                current.push(Value::Object(assistant));
                push_prefix(&mut prefixes, &current, Some(&mut position));

                if !reasoning.is_empty() {
                    let object = current[assistant_index].as_object_mut().unwrap();
                    for (key, value) in reasoning {
                        object.insert(key.to_string(), value);
                    }
                    if message.reasoning().is_some()
                        || message
                            .extra
                            .get("reasoning_details")
                            .and_then(Value::as_array)
                            .is_some_and(|blocks| !blocks.is_empty())
                    {
                        push_prefix(&mut prefixes, &current, Some(&mut position));
                    }
                }

                if let Some(Value::Array(all_calls)) = original_calls {
                    let mut progressive = Vec::new();
                    for (call_index, call) in calls.iter().enumerate() {
                        progressive.push(all_calls[call_index].clone());
                        current[assistant_index]
                            .as_object_mut()
                            .unwrap()
                            .insert("tool_calls".into(), Value::Array(progressive.clone()));
                        append_results(
                            slice,
                            std::slice::from_ref(call),
                            &mut current,
                            &mut consumed_results,
                        );
                        let visible = (!is_synthetic(call)).then_some(&mut position);
                        push_prefix(&mut prefixes, &current, visible);
                    }
                }
            }
            Role::Tool => {
                current.push(json!(message));
                push_prefix(&mut prefixes, &current, Some(&mut position));
            }
        }
    }

    let expected: Vec<Value> = slice.iter().map(|message| json!(message)).collect();
    let actual = if injection {
        &current[1..]
    } else {
        &current[..]
    };
    ensure!(
        actual == expected,
        "turn prefix construction changed message order"
    );
    if prefixes
        .last()
        .is_none_or(|prefix| prefix.messages != current)
    {
        push_prefix(&mut prefixes, &current, None);
    }
    let digest = digest(endpoint, slice)?;
    Ok(Plan {
        range,
        digest,
        prefixes,
    })
}

/// Append the one result coupled to each call, retaining durable result order.
fn append_results(
    slice: &[Message],
    calls: &[crate::session::ToolCall],
    current: &mut Vec<Value>,
    consumed: &mut HashSet<usize>,
) {
    for call in calls {
        if let Some((index, result)) = slice.iter().enumerate().find(|(index, message)| {
            !consumed.contains(index) && message.tool_call_id.as_deref() == Some(call.id.as_str())
        }) {
            current.push(json!(result));
            consumed.insert(index);
        }
    }
}

/// Snapshot after a semantic boundary; visible snapshots receive consecutive positions.
fn push_prefix(prefixes: &mut Vec<Prefix>, messages: &[Value], position: Option<&mut usize>) {
    let position = position.map(|position| {
        let value = *position;
        *position += 1;
        value
    });
    prefixes.push(Prefix {
        messages: messages.to_vec(),
        position,
    });
}

/// App-generated synthetic calls affect totals but have no compact display position.
fn is_synthetic(call: &crate::session::ToolCall) -> bool {
    tool_call_id::decode(&call.id).is_some_and(|metadata| metadata.kind == tool_call_id::SYNTHETIC)
}

/// Cache identity includes every configured or durable input that affects tokenization.
fn digest(endpoint: &ResolvedEndpoint, messages: &[Message]) -> anyhow::Result<String> {
    let mut hash = Sha256::new();
    hash.update(DIGEST_DOMAIN);
    hash.update(endpoint.tokenize_url.as_bytes());
    hash.update([0]);
    hash.update(endpoint.model.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(messages).context("serializing turn token cache key")?);
    Ok(format!("sha256:{:x}", hash.finalize()))
}

/// The exact integer form of ceil(compact UTF-8 bytes / 3.25).
fn estimated(plan: &Plan) -> TurnTokenCache {
    let counts = plan
        .prefixes
        .iter()
        .map(|prefix| {
            let bytes = serde_json::to_vec(&prefix.messages)
                .expect("serializing already-serializable token prefix")
                .len() as u64;
            bytes.saturating_mul(4).saturating_add(12) / 13
        })
        .collect();
    from_prefix_counts(plan, counts, true)
}

/// Hidden synthetic prefixes advance the baseline without creating a displayed count.
fn from_prefix_counts(plan: &Plan, counts: Vec<u64>, estimated: bool) -> TurnTokenCache {
    let mut previous = 0;
    let mut positions = vec![0; plan.position_count()];
    for (prefix, count) in plan.prefixes.iter().zip(counts.iter().copied()) {
        if let Some(position) = prefix.position {
            positions[position] = count.saturating_sub(previous);
        }
        previous = count;
    }
    TurnTokenCache {
        digest: plan.digest.clone(),
        total: previous,
        positions,
        estimated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    /// Use a stable tokenizer identity; no test in this module makes network requests.
    fn endpoint() -> ResolvedEndpoint {
        ResolvedEndpoint {
            url: "http://example.test/v1/chat/completions".into(),
            tokenize_url: "http://example.test/tokenize".into(),
            model: "test-model".into(),
            api_key: None,
            parameters: Map::new(),
        }
    }

    /// User turns and marked file pairs get independent ranges and matching positions.
    #[test]
    fn turn_and_position_boundaries() {
        let injection_id =
            crate::tool_call_id::generate(crate::tool_call_id::INJECTED_FILE, &[]).unwrap();
        let synthetic_id =
            crate::tool_call_id::generate(crate::tool_call_id::SYNTHETIC, &[]).unwrap();
        let messages: Vec<Message> = serde_json::from_value(json!([
            {"role":"assistant","content":"","tool_calls":[{"id":injection_id,"type":"function","function":{"name":"REPL","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":injection_id,"content":"file"},
            {"role":"user","content":"question"},
            {"role":"assistant","content":"","tool_calls":[{"id":synthetic_id,"type":"function","function":{"name":"REPL","arguments":"{\"code\":\"FYI()\"}"}}]},
            {"role":"tool","tool_call_id":synthetic_id,"content":"FYI"},
            {"role":"assistant","content":"answer","reasoning":"thought"}
        ])).unwrap();
        let plans = plans(&messages, &endpoint()).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].range, 0..2);
        assert_eq!(plans[0].position_count(), 2);
        assert_eq!(plans[1].range, 2..6);
        assert_eq!(plans[1].position_count(), 3);
        let counted = from_prefix_counts(&plans[1], vec![10, 20, 30, 40], false);
        assert_eq!(counted.positions, [10, 10, 10]);
        assert_eq!(counted.total, 40);
        let injection = estimated(&plans[0]);
        assert_eq!(injection.positions.len(), 2);
        assert!(injection.estimated);
        assert!(injection.total >= injection.positions.iter().sum());
    }

    /// Digest identity changes with durable content and tokenizer identity.
    #[test]
    fn cache_identity_and_reuse() {
        let messages = vec![Message::user("hello".into())];
        let endpoint = endpoint();
        let plan = plans(&messages, &endpoint).unwrap().remove(0);
        let estimate = estimated(&plan);
        assert_eq!(
            cached_or_estimated(&plan, std::slice::from_ref(&estimate)),
            estimate
        );

        let changed = plans(&[Message::user("changed".into())], &endpoint)
            .unwrap()
            .remove(0);
        assert_ne!(changed.digest, plan.digest);
        assert_ne!(
            cached_or_estimated(&changed, &[estimate]).digest,
            plan.digest
        );
    }

    /// Session totals sum every overlay turn and inherit any heuristic input.
    #[test]
    fn total_combines_turn_counts_and_estimate_state() {
        let mut counter = Counter::new(reqwest::Client::new(), endpoint());
        assert_eq!(
            counter.total(),
            TokenTotal {
                count: 0,
                estimated: false,
            }
        );
        counter.turns = vec![
            CountedTurn {
                range: 0..1,
                cache: TurnTokenCache {
                    digest: "first".into(),
                    total: 40,
                    positions: vec![],
                    estimated: false,
                },
            },
            CountedTurn {
                range: 1..2,
                cache: TurnTokenCache {
                    digest: "second".into(),
                    total: 2,
                    positions: vec![],
                    estimated: true,
                },
            },
        ];
        assert_eq!(
            counter.total(),
            TokenTotal {
                count: 42,
                estimated: true,
            }
        );
    }

    /// Ending a turn queues its unchanged message revision for provider tokenization.
    #[test]
    fn active_turn_transition_queues_completed_plan() {
        let mut session = crate::session::Session::new(std::path::Path::new("/unused"));
        session.messages = vec![
            Message::user("question".into()),
            Message {
                role: Role::Assistant,
                content: "answer".into(),
                ..Default::default()
            },
        ];
        let endpoint = endpoint();
        let mut counter = Counter::new(reqwest::Client::new(), endpoint);
        counter.refresh(&mut session, Some(0)).unwrap();
        assert!(counter.pending.is_empty());
        counter.refresh(&mut session, None).unwrap();
        assert_eq!(counter.pending.len(), 1);
    }
}
