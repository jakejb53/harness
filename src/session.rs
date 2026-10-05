//! Session turns are the durable conversation, before request-time context filtering
//! and tool reasoning. Messages are appended within turns, except that resume inserts results for
//! unanswered tool calls, and each turn removes earlier `FYI()`/`help()`-only calls
//! (`remove_calls`).
//! Partial replies are kept as-is, except that a tool call still
//! streaming when the reply ended is dropped. Assistant messages keep every field
//! the endpoint streamed, including ones the app doesn't know.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sessions directory, inside the harness directory. A session's name is its file
/// name without `.json`.
const SESSIONS_DIR: &str = "sessions";

/// Result recorded for a complete tool call that never ran.
pub const NOT_RUN: &str = "[not run: turn stopped]";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Complete tool calls made by an assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// The call a `tool` message answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Every other field of an assistant message, exactly as the endpoint streamed
    /// it: reasoning under whatever name the endpoint uses, and fields the app doesn't
    /// know. Sent back unchanged, so the endpoint gets its own format.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Message {
    pub fn user(content: String) -> Self {
        Message {
            role: Role::User,
            content,
            ..Default::default()
        }
    }

    pub fn tool(tool_call_id: String, content: String) -> Self {
        Message {
            role: Role::Tool,
            content,
            tool_call_id: Some(tool_call_id),
            ..Default::default()
        }
    }

    /// Reasoning to display, from whichever field the endpoint used.
    pub fn reasoning(&self) -> Option<Cow<'_, str>> {
        reasoning_text(&self.extra)
    }

    /// Nothing worth keeping: no content, no calls, and no other field with a value.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
            && self.tool_calls.as_ref().is_none_or(Vec::is_empty)
            && !self.extra.values().any(has_value)
    }
}

/// Display text of the reasoning in an assistant message's fields: `reasoning`, else
/// `reasoning_content`, else the text of the `reasoning_details` blocks (encrypted
/// blocks have none). Endpoints that send more than one carry the same reasoning in
/// each, so only one is shown.
pub fn reasoning_text(fields: &Map<String, Value>) -> Option<Cow<'_, str>> {
    for key in ["reasoning", "reasoning_content"] {
        if let Some(text) = fields.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            return Some(Cow::Borrowed(text));
        }
    }
    let text = fields
        .get("reasoning_details")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text").or_else(|| block.get("summary"))?.as_str())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(Cow::Owned(text))
}

/// Streams often carry placeholder fields (`"refusal": null`, `"reasoning": ""`);
/// those alone don't make a message worth keeping.
fn has_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"`.
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
    /// Fields the app doesn't use, kept as streamed and sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn function_kind() -> String {
    "function".to_string()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, exactly as the model produced them.
    #[serde(default)]
    pub arguments: String,
    /// Fields the app doesn't use, kept as streamed and sent back unchanged.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Cached derived data is local to Harness and never enters provider requests.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct LocalMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_token_cache: Vec<TurnTokenCache>,
}

impl LocalMetadata {
    /// Empty metadata is omitted so ordinary session files retain their minimal shape.
    fn is_empty(&self) -> bool {
        self.turn_token_cache.is_empty()
    }
}

/// One complete tokenization result, keyed by every input that affects it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TurnTokenCache {
    pub digest: String,
    pub total: u64,
    pub positions: Vec<u64>,
    pub estimated: bool,
}

/// One context-management unit. Hidden turns remain durable and visible locally but
/// are omitted when provider requests are built.
#[derive(Clone)]
pub struct Turn {
    pub messages: Vec<Message>,
    pub hidden: bool,
}

impl Turn {
    /// File injections are dedicated two-message turns identified by their encoded call ID.
    pub fn is_injection(&self) -> bool {
        self.messages.len() == 2 && is_injection_start(&self.messages, 0)
    }
}

/// A local marker follows the turn it hides inside the saved `messages` stream.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnMarkerEnvelope {
    #[serde(rename = "_harness_local")]
    local: TurnMarker,
}

/// `false` is accepted as an unambiguous no-op but never emitted.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnMarker {
    hidden: bool,
}

/// Session files interleave provider messages with Harness-only turn markers.
enum SessionEntry {
    Marker(TurnMarkerEnvelope),
    Message(Message),
}

impl<'de> Deserialize<'de> for SessionEntry {
    /// Distinguish local markers before typed decoding so malformed markers retain
    /// a marker-specific error instead of Serde's generic untagged-enum failure.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let marker = value.get("_harness_local").is_some() && value.get("role").is_none();
        if marker {
            serde_json::from_value(value)
                .map(SessionEntry::Marker)
                .map_err(|error| {
                    serde::de::Error::custom(format!("invalid hidden-turn marker: {error}"))
                })
        } else {
            serde_json::from_value(value)
                .map(SessionEntry::Message)
                .map_err(|error| {
                    serde::de::Error::custom(format!("invalid provider message: {error}"))
                })
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionFile {
    messages: Vec<SessionEntry>,
    #[serde(default, rename = "_harness_local")]
    local: LocalMetadata,
}

/// Serialization twin of `SessionEntry` that borrows provider messages.
#[derive(Serialize)]
#[serde(untagged)]
enum SessionEntryRef<'a> {
    Message(&'a Message),
    Marker(TurnMarkerEnvelope),
}

/// Serialization twin of `SessionFile` that owns only the lightweight entry list.
#[derive(Serialize)]
struct SessionFileRef<'a> {
    messages: Vec<SessionEntryRef<'a>>,
    #[serde(
        rename = "_harness_local",
        skip_serializing_if = "LocalMetadata::is_empty"
    )]
    local: &'a LocalMetadata,
}

pub struct Session {
    pub path: PathBuf,
    pub turns: Vec<Turn>,
    pub local: LocalMetadata,
    revision: u64,
}

/// Session file name for `name`. Names must stay inside the sessions directory and
/// not collide with hidden or temp files: not empty, no `/`, no leading `.`.
fn file_name(name: &str) -> anyhow::Result<String> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid session name: {name:?}");
    }
    Ok(format!("{name}.json"))
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
        && crate::tool_call_id::decode(&call.id)
            .is_some_and(|metadata| metadata.kind == crate::tool_call_id::INJECTED_FILE)
        && messages.get(index + 1).is_some_and(|result| {
            result.role == Role::Tool && result.tool_call_id.as_deref() == Some(call.id.as_str())
        })
}

/// Split provider messages into user turns and standalone marked file injections.
fn turn_ranges(messages: &[Message]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut index = 0;
    while index < messages.len() {
        if is_injection_start(messages, index) {
            if let Some(first) = start.take() {
                ranges.push(first..index);
            }
            ranges.push(index..index + 2);
            index += 2;
            continue;
        }
        if messages[index].role == Role::User {
            if let Some(first) = start.replace(index) {
                ranges.push(first..index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
        index += 1;
    }
    if let Some(first) = start {
        ranges.push(first..messages.len());
    }
    ranges
        .into_iter()
        .filter(|range| !range.is_empty())
        .collect()
}

/// Resolve local markers against derived turn boundaries without guessing placement.
fn turns_from_entries(entries: Vec<SessionEntry>) -> anyhow::Result<Vec<Turn>> {
    let mut messages = Vec::new();
    let mut markers = BTreeMap::new();
    for entry in entries {
        match entry {
            SessionEntry::Message(message) => messages.push(message),
            SessionEntry::Marker(marker) => {
                ensure!(
                    markers
                        .insert(messages.len(), marker.local.hidden)
                        .is_none(),
                    "duplicate hidden-turn marker after message {}",
                    messages.len()
                );
            }
        }
    }

    let ranges = turn_ranges(&messages);
    for position in markers.keys() {
        ensure!(
            ranges.iter().any(|range| range.end == *position),
            "hidden-turn marker after message {position} does not follow a complete turn"
        );
    }
    Ok(ranges
        .into_iter()
        .map(|range| Turn {
            hidden: markers.get(&range.end).copied().unwrap_or(false),
            messages: messages[range].to_vec(),
        })
        .collect())
}

/// Names of saved sessions, least recently changed first. No sessions directory
/// means no sessions.
pub fn list(harness_dir: &Path) -> anyhow::Result<Vec<String>> {
    let dir = harness_dir.join(SESSIONS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        // Skips `.json.tmp` files left by an interrupted save.
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .with_context(|| format!("reading {}", path.display()))?;
        sessions.push((modified, name.to_string()));
    }
    sessions.sort();
    Ok(sessions.into_iter().map(|(_, name)| name).collect())
}

impl Session {
    /// A new session, named by its start time. The file is not created until the
    /// first `save`, so a launch with no messages leaves nothing behind.
    pub fn new(harness_dir: &Path) -> Self {
        let name = chrono::Local::now().format("%Y%m%d-%H%M%S");
        Session {
            path: harness_dir.join(SESSIONS_DIR).join(format!("{name}.json")),
            turns: Vec::new(),
            local: LocalMetadata::default(),
            revision: 0,
        }
    }

    /// Load session `name`. Tool calls left without results (the app was killed or
    /// crashed mid-turn) get `NOT_RUN` results, saved immediately, since the endpoint
    /// rejects every request while any are missing. Returns how many were added.
    pub fn resume(harness_dir: &Path, name: &str) -> anyhow::Result<(Self, usize)> {
        let path = harness_dir.join(SESSIONS_DIR).join(file_name(name)?);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: SessionFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let turns = turns_from_entries(file.messages)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut session = Session {
            path,
            turns,
            local: file.local,
            revision: 0,
        };
        let repaired = session.answer_unanswered_calls();
        if repaired > 0 {
            session.revision = session.revision.wrapping_add(1);
            session.save()?;
        }
        Ok((session, repaired))
    }

    /// Every durable provider message in transcript order, including hidden turns.
    pub fn messages(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.turns.iter().flat_map(|turn| turn.messages.iter())
    }

    /// Provider messages selected by the context editor, copied for request-time rewrites.
    pub fn visible_messages(&self) -> Vec<Message> {
        self.turns
            .iter()
            .filter(|turn| !turn.hidden)
            .flat_map(|turn| turn.messages.iter().cloned())
            .collect()
    }

    /// Flat message count used by transcript note anchors.
    pub fn message_count(&self) -> usize {
        self.turns.iter().map(|turn| turn.messages.len()).sum()
    }

    /// Persist one completed turn's visibility as an adjacent local marker.
    pub fn toggle_hidden(&mut self, turn_index: usize) -> anyhow::Result<()> {
        let turn = self
            .turns
            .get_mut(turn_index)
            .with_context(|| format!("turn {turn_index} does not exist"))?;
        turn.hidden = !turn.hidden;
        if let Err(error) = self.save() {
            self.turns[turn_index].hidden = !self.turns[turn_index].hidden;
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    /// Insert a `NOT_RUN` result for each tool call without one, after the tool
    /// messages that follow its assistant message.
    fn answer_unanswered_calls(&mut self) -> usize {
        let mut added = 0;
        for turn in &mut self.turns {
            let mut i = 0;
            while i < turn.messages.len() {
                let Some(calls) = &turn.messages[i].tool_calls else {
                    i += 1;
                    continue;
                };
                let ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
                let mut end = i + 1;
                let mut answered = HashSet::new();
                while let Some(message) = turn.messages.get(end)
                    && message.role == Role::Tool
                {
                    answered.extend(message.tool_call_id.clone());
                    end += 1;
                }
                let missing: Vec<Message> = ids
                    .into_iter()
                    .filter(|id| !answered.contains(id))
                    .map(|id| Message::tool(id, NOT_RUN.to_string()))
                    .collect();
                added += missing.len();
                let inserted = missing.len();
                turn.messages.splice(end..end, missing);
                i = end + inserted;
            }
        }
        added
    }

    /// Append a model/tool message and immediately persist the conversation.
    pub fn push(&mut self, message: Message) -> anyhow::Result<()> {
        self.turns
            .last_mut()
            .context("cannot append a message without an active turn")?
            .messages
            .push(message);
        self.revision = self.revision.wrapping_add(1);
        self.save()
    }

    /// Persist an explicit user submission without retaining it in memory on save failure.
    pub fn submit(&mut self, text: String) -> anyhow::Result<()> {
        self.turns.push(Turn {
            messages: vec![Message::user(text)],
            hidden: false,
        });
        if let Err(error) = self.save() {
            self.turns.pop();
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    /// Attach completed local reads without a user message or any turn-start calls.
    pub fn inject(
        &mut self,
        prepared: Vec<crate::injection::PreparedInjection>,
    ) -> anyhow::Result<()> {
        let turn_count = self.turns.len();
        for injection in prepared {
            let id = injection.call.id.clone();
            self.turns.push(Turn {
                messages: vec![
                    Message {
                        role: Role::Assistant,
                        tool_calls: Some(vec![injection.call]),
                        ..Default::default()
                    },
                    Message::tool(id, injection.content),
                ],
                hidden: false,
            });
        }
        if let Err(error) = self.save() {
            self.turns.truncate(turn_count);
            return Err(error);
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    /// Keep marked attachments exclusive and paired, even without a removal control.
    pub fn validate_injections(&self) -> anyhow::Result<()> {
        let messages: Vec<_> = self.messages().collect();
        let mut base = 0;
        for turn in &self.turns {
            for (offset, message) in turn.messages.iter().enumerate() {
                let Some(calls) = &message.tool_calls else {
                    continue;
                };
                for call in calls {
                    if crate::tool_call_id::decode(&call.id)
                        .is_none_or(|metadata| metadata.kind != crate::tool_call_id::INJECTED_FILE)
                    {
                        continue;
                    }
                    // A marked call identifies one whole attachment turn, not a mixed message.
                    let call_count = messages
                        .iter()
                        .filter_map(|entry| entry.tool_calls.as_ref())
                        .flatten()
                        .filter(|entry| entry.id == call.id)
                        .count();
                    if call_count != 1
                        || turn.messages.len() != 2
                        || offset != 0
                        || message.role != Role::Assistant
                        || !message.content.is_empty()
                        || !message.extra.is_empty()
                        || calls.len() != 1
                        || call.function.name != "REPL"
                    {
                        bail!(
                            "invalid file injection {:?}: expected one dedicated REPL call",
                            call.id
                        );
                    }
                    let results: Vec<_> = messages
                        .iter()
                        .enumerate()
                        .filter(|(_, entry)| {
                            entry.tool_call_id.as_deref() == Some(call.id.as_str())
                        })
                        .collect();
                    if results.len() != 1
                        || results[0].0 != base + offset + 1
                        || results[0].1.role != Role::Tool
                    {
                        bail!(
                            "invalid file injection {:?}: expected its result immediately after the call",
                            call.id
                        );
                    }
                    crate::injection::filename(call)?;
                }
            }
            base += turn.messages.len();
        }
        Ok(())
    }

    /// `/rename`: rename the session file, or before the first save, the file it will
    /// be created as. An existing session is never overwritten.
    pub fn rename(&mut self, name: &str) -> anyhow::Result<()> {
        let path = self.path.with_file_name(file_name(name)?);
        if path == self.path {
            return Ok(());
        }
        if path.exists() {
            bail!("session {name} already exists");
        }
        if self.path.exists() {
            std::fs::rename(&self.path, &path).with_context(|| {
                format!("renaming {} to {}", self.path.display(), path.display())
            })?;
        }
        self.path = path;
        Ok(())
    }

    /// Remove untracked tool calls matching `matches`, with results, and save if
    /// anything changed. An assistant message left empty (`Message::is_empty`) is
    /// removed too; one that still has content or another field stays without the
    /// call. Returns the pre-removal indices of the removed messages, ascending.
    pub fn remove_calls(
        &mut self,
        matches: impl Fn(&ToolCall) -> bool,
    ) -> anyhow::Result<Vec<usize>> {
        let mut removed_ids = HashSet::new();
        let mut emptied = Vec::with_capacity(self.turns.len());
        for turn in &mut self.turns {
            let mut turn_emptied = vec![false; turn.messages.len()];
            for (message, emptied) in turn.messages.iter_mut().zip(&mut turn_emptied) {
                let Some(calls) = &mut message.tool_calls else {
                    continue;
                };
                calls.retain(|call| {
                    // Only explicit instance removal may delete a user attachment.
                    let injected = crate::tool_call_id::decode(&call.id).is_some_and(|metadata| {
                        metadata.kind == crate::tool_call_id::INJECTED_FILE
                    });
                    let remove = !injected && matches(call);
                    if remove {
                        removed_ids.insert(call.id.clone());
                    }
                    !remove
                });
                if calls.is_empty() {
                    message.tool_calls = None;
                    *emptied = message.is_empty();
                }
            }
            emptied.push(turn_emptied);
        }
        if removed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        let mut index = 0;
        for (turn, emptied) in self.turns.iter_mut().zip(emptied) {
            let mut emptied = emptied.into_iter();
            turn.messages.retain(|message| {
                let emptied = emptied.next().unwrap_or(false);
                let orphaned_result = message.role == Role::Tool
                    && message
                        .tool_call_id
                        .as_ref()
                        .is_some_and(|id| removed_ids.contains(id));
                let keep = !emptied && !orphaned_result;
                if !keep {
                    removed.push(index);
                }
                index += 1;
                keep
            });
        }
        self.turns.retain(|turn| !turn.messages.is_empty());
        self.revision = self.revision.wrapping_add(1);
        self.save()?;
        Ok(removed)
    }

    /// Monotonic marker for changes to durable messages or model-visible selection.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace derived token data without changing the provider-visible revision.
    pub fn save_token_cache(&mut self, cache: Vec<TurnTokenCache>) -> anyhow::Result<()> {
        self.local.turn_token_cache = cache;
        self.save()
    }

    /// Write via a synced temp file and rename so a crash or power loss never leaves a
    /// half-written file.
    fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut messages = Vec::with_capacity(
            self.message_count() + self.turns.iter().filter(|turn| turn.hidden).count(),
        );
        for turn in &self.turns {
            messages.extend(turn.messages.iter().map(SessionEntryRef::Message));
            if turn.hidden {
                messages.push(SessionEntryRef::Marker(TurnMarkerEnvelope {
                    local: TurnMarker { hidden: true },
                }));
            }
        }
        let json = serde_json::to_string_pretty(&SessionFileRef {
            messages,
            local: &self.local,
        })?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), self.path.display()))
    }
}

#[cfg(test)]
mod injection_tests {
    use super::*;
    use crate::injection::PreparedInjection;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        /// Isolate persisted sessions under a unique scratch directory.
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "harness-session-injection-{}-{}-{}",
                std::process::id(),
                chrono::Local::now().timestamp_nanos_opt().unwrap(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        /// Use a fixed session name within this fixture for resume assertions.
        fn session(&self) -> Session {
            let mut session = Session::new(&self.0);
            session.path = self.0.join("sessions/test.json");
            session
        }

        /// Exercise public resume rather than deserializing test state directly.
        fn resume(&self) -> anyhow::Result<(Session, usize)> {
            Session::resume(&self.0, "test")
        }
    }

    impl Drop for Fixture {
        /// Delete only scratch state owned by this test.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Construct a marked completed read without accessing any source file.
    fn prepared(content: &str) -> PreparedInjection {
        PreparedInjection {
            call: ToolCall {
                id: crate::tool_call_id::generate(crate::tool_call_id::INJECTED_FILE, &[]).unwrap(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "REPL".into(),
                    arguments: json!({"code": "from pathlib import Path\nprint(Path(\"missing.txt\").read_bytes().decode('utf-8'), end='')"}).to_string(),
                    extra: Map::new(),
                },
                extra: Map::new(),
            },
            content: content.into(),
        }
    }

    /// Borrow the flat provider-message view used by transcript and request tests.
    fn messages(session: &Session) -> Vec<&Message> {
        session.messages().collect()
    }

    /// Compare the provider-message state independently of local caches.
    fn state(session: &Session) -> Value {
        json!({"messages": messages(session)})
    }

    /// Duplicate injections and full results survive resume as saved messages.
    #[test]
    fn marked_duplicate_instances_persist_as_messages_only() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        let large = "full contents\r\n".repeat(20_000);
        session
            .inject(vec![prepared(&large), prepared("")])
            .unwrap();
        let current = messages(&session);
        assert_eq!(current.len(), 4);
        assert_eq!(current[1].content, large);
        assert_eq!(current[3].content, "");
        let first = &current[0].tool_calls.as_ref().unwrap()[0].id;
        let second = &current[2].tool_calls.as_ref().unwrap()[0].id;
        assert_ne!(first, second);
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert_eq!(saved, state(&session));
        assert_eq!(saved.as_object().unwrap().len(), 1);
        let (resumed, repaired) = fixture.resume().unwrap();
        assert_eq!(repaired, 0);
        assert_eq!(state(&resumed), saved);
        for enabled in [false, true] {
            let visible = resumed.visible_messages();
            assert_eq!(
                json!(crate::tool_reasoning::request_messages(&visible, enabled)),
                saved["messages"]
            );
        }
        session.submit("Review these".into()).unwrap();
        let current = messages(&session);
        assert!(current[4].role == Role::User);
        assert_eq!(json!(&current[..4]), saved["messages"]);
    }

    /// Hidden state is adjacent to its turn, survives resume, and never reaches requests.
    #[test]
    fn hidden_turn_marker_round_trips_and_filters_requests() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("first".into()).unwrap();
        session
            .push(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "hidden-call".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "REPL".into(),
                        arguments: json!({"code":"print(1)"}).to_string(),
                        extra: Map::new(),
                    },
                    extra: Map::new(),
                }]),
                extra: Map::from_iter([("reasoning".into(), json!("private thought"))]),
                ..Default::default()
            })
            .unwrap();
        session
            .push(Message::tool("hidden-call".into(), "1".into()))
            .unwrap();
        session.submit("second".into()).unwrap();
        session.toggle_hidden(0).unwrap();

        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert_eq!(
            saved["messages"][3],
            json!({"_harness_local":{"hidden":true}})
        );
        assert_eq!(session.visible_messages().len(), 1);
        assert_eq!(session.visible_messages()[0].content, "second");
        for enabled in [false, true] {
            assert_eq!(
                crate::tool_reasoning::request_messages(&session.visible_messages(), enabled),
                [json!({"role":"user","content":"second"})]
            );
        }

        let mut resumed = fixture.resume().unwrap().0;
        assert!(resumed.turns[0].hidden);
        assert!(!resumed.turns[1].hidden);
        resumed.toggle_hidden(0).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&resumed.path).unwrap()).unwrap();
        assert!(
            saved["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry.get("_harness_local").is_none())
        );
    }

    /// Standalone file-injection turns use the same persistent visibility marker.
    #[test]
    fn injected_file_turn_can_be_hidden() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.inject(vec![prepared("file contents")]).unwrap();
        session.toggle_hidden(0).unwrap();
        assert!(session.visible_messages().is_empty());

        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert_eq!(
            saved["messages"][2],
            json!({"_harness_local":{"hidden":true}})
        );
        let resumed = fixture.resume().unwrap().0;
        assert!(resumed.turns[0].is_injection());
        assert!(resumed.turns[0].hidden);
    }

    /// Markers must follow exactly one derived turn; unmarked sessions stay fully visible.
    #[test]
    fn marker_validation_preserves_unmarked_sessions() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("message".into()).unwrap();
        assert!(!fixture.resume().unwrap().0.turns[0].hidden);

        std::fs::write(
            &session.path,
            json!({"messages":[
                {"role":"user","content":"message"},
                {"_harness_local":{"hidden":false}}
            ]})
            .to_string(),
        )
        .unwrap();
        let visible = fixture.resume().unwrap().0;
        assert!(!visible.turns[0].hidden);
        visible.save().unwrap();
        let canonical: Value =
            serde_json::from_str(&std::fs::read_to_string(&visible.path).unwrap()).unwrap();
        assert_eq!(canonical["messages"].as_array().unwrap().len(), 1);

        for messages in [
            json!([
                {"_harness_local":{"hidden":true}},
                {"role":"user","content":"message"}
            ]),
            json!([
                {"role":"user","content":"message"},
                {"_harness_local":{"hidden":true}},
                {"_harness_local":{"hidden":true}}
            ]),
        ] {
            std::fs::write(&session.path, json!({"messages":messages}).to_string()).unwrap();
            assert!(fixture.resume().is_err());
        }

        std::fs::write(
            &session.path,
            json!({"messages":[
                {"role":"user","content":"message"},
                {"_harness_local":{"hidden":"yes"}}
            ]})
            .to_string(),
        )
        .unwrap();
        let error = format!("{:#}", fixture.resume().err().unwrap());
        assert!(error.contains("invalid hidden-turn marker"), "{error}");
    }

    /// Unknown top-level and local metadata fail at the persistence boundary.
    #[test]
    fn unknown_session_metadata_is_rejected() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("message".into()).unwrap();
        let messages = json!(messages(&session));
        for invalid in [
            json!({"messages": messages, "injections": []}),
            json!({"messages": messages, "_harness_local": {"unknown": true}}),
        ] {
            std::fs::write(&session.path, invalid.to_string()).unwrap();
            assert!(fixture.resume().is_err());
        }
    }

    /// Local cache persists beside messages and remains outside provider-visible state.
    #[test]
    fn token_cache_round_trips_under_local_namespace() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("message".into()).unwrap();
        let cache = TurnTokenCache {
            digest: "sha256:test".into(),
            total: 42,
            positions: vec![42],
            estimated: false,
        };
        session.save_token_cache(vec![cache.clone()]).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert_eq!(saved["_harness_local"]["turn_token_cache"][0]["total"], 42);
        let resumed = fixture.resume().unwrap().0;
        assert_eq!(resumed.local.turn_token_cache, [cache]);
        assert_eq!(json!(messages(&resumed)), saved["messages"]);
    }

    /// Even a broad automatic cleanup preserves instances carrying the injection marker.
    #[test]
    fn cleanup_preserves_marked_calls() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.inject(vec![prepared("file")]).unwrap();
        let id = messages(&session)[0].tool_calls.as_ref().unwrap()[0]
            .id
            .clone();
        let mut normal = prepared("environment");
        normal.call.id = "fyi-test".into();
        normal.call.function.arguments = json!({"code": "FYI()"}).to_string();
        session.inject(vec![normal]).unwrap();
        assert_eq!(session.remove_calls(|_| true).unwrap(), [2, 3]);
        assert_eq!(messages(&session)[0].tool_calls.as_ref().unwrap()[0].id, id);
        assert_eq!(state(&fixture.resume().unwrap().0), state(&session));
    }

    /// Failed injection and submission saves preserve the in-memory conversation.
    #[test]
    fn failed_saves_preserve_memory() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("earlier".into()).unwrap();
        session
            .inject(vec![prepared("one"), prepared("two"), prepared("three")])
            .unwrap();
        session.submit("later".into()).unwrap();
        let current = messages(&session);
        assert_eq!(current[0].content, "earlier");
        assert_eq!(current[7].content, "later");
        let before = state(&session);
        assert_eq!(state(&fixture.resume().unwrap().0), before);
        let blocked = fixture.0.join("not-a-directory");
        std::fs::write(&blocked, "blocked").unwrap();
        session.path = blocked.join("session.json");
        assert!(session.inject(vec![prepared("new")]).is_err());
        assert_eq!(state(&session), before);
        assert!(session.submit("new draft".into()).is_err());
        assert_eq!(state(&session), before);
        assert_eq!(state(&fixture.resume().unwrap().0), before);
    }

    /// Marked attachments remain exclusive, paired, and decodable after UI removal goes away.
    #[test]
    fn marked_attachment_validation_rejects_malformed_pairs() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.inject(vec![prepared("contents")]).unwrap();
        let original = state(&session);
        assert!(session.validate_injections().is_ok());
        for defect in 0..4 {
            let mut invalid = original.clone();
            match defect {
                0 => {
                    invalid["messages"].as_array_mut().unwrap().pop();
                }
                1 => invalid["messages"][0]["content"] = json!("mixed content"),
                2 => invalid["messages"]
                    .as_array_mut()
                    .unwrap()
                    .push(original["messages"][1].clone()),
                3 => {
                    invalid["messages"][0]["tool_calls"][0]["function"]["arguments"] =
                        json!("not JSON")
                }
                _ => unreachable!(),
            }
            let entries: Vec<SessionEntry> =
                serde_json::from_value(invalid["messages"].clone()).unwrap();
            session.turns = turns_from_entries(entries).unwrap();
            assert!(
                session.validate_injections().is_err(),
                "accepted defect {defect}"
            );
        }
    }
}
