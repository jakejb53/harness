//! Session turns are the durable conversation, before request-time context filtering.
//! Messages are appended within turns, except that resume inserts results for
//! unanswered tool calls, and each turn removes earlier `FYI` tool calls and earlier
//! REPL calls made only of `FYI()`/`help()` (`remove_calls`).
//! Partial replies are kept as-is, except that a tool call still
//! streaming when the reply ended is dropped. Assistant messages keep every field
//! the endpoint streamed, including ones the app doesn't know.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Sessions directory, inside the harness directory. Each tree is one directory,
/// `sessions/<name>/`: the root is `library/root.json`, depth 1 branches are
/// `library/<call-id>.json`, and deeper branches are `deep/<call-id>.json`. Anything
/// else under `sessions/`, such as a pre-RLM flat `<name>.json`, is not a session.
const SESSIONS_DIR: &str = "sessions";

/// Tree subdirectory holding the root and depth 1 branches.
const LIBRARY_DIR: &str = "library";

/// Tree subdirectory holding branches at depth 2 and deeper.
const DEEP_DIR: &str = "deep";

/// The root session's ID: its file stem, and the `parent` its depth 1 branches record.
const ROOT_ID: &str = "root";

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

/// Harness-only session data that never enters provider requests. `repository`,
/// `depth`, and `parent` are set when the session is created and never rewritten, so
/// they are private to this module; only the derived token cache changes.
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LocalMetadata {
    /// Canonical launch directory of the process that created the session. Recorded
    /// only; resume never compares it with the current launch directory.
    repository: String,
    /// 0 for the root; a branch is one deeper than the agent that spawned it.
    depth: u32,
    /// Absent at depth 0; `root` at depth 1; the parent's call ID deeper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_token_cache: Vec<TurnTokenCache>,
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
    // Required: a file without `repository` and `depth` (such as a pre-RLM session)
    // is not a session.
    #[serde(rename = "_harness_local")]
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
    #[serde(rename = "_harness_local")]
    local: &'a LocalMetadata,
}

/// One session file in a tree. Its location is derived from the tree name, its ID,
/// and its depth (`path`), never stored separately.
pub struct Session {
    /// The harness's `sessions/` directory.
    sessions_dir: PathBuf,
    /// The tree directory's name under `sessions_dir`; `/rename` changes it.
    tree: String,
    /// `ROOT_ID` for the root, else the branch's call ID. Names the file.
    id: String,
    pub turns: Vec<Turn>,
    pub local: LocalMetadata,
    revision: u64,
}

/// Tree names must stay inside the sessions directory and not collide with hidden or
/// temp entries: not empty, no `/`, no leading `.`.
fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid session name: {name:?}");
    }
    Ok(())
}

/// The file of session `id` at `depth` in tree directory `tree`: the root and depth 1
/// branches are in `library/`, deeper branches in `deep/`.
fn file_path(tree: &Path, depth: u32, id: &str) -> PathBuf {
    let subdir = if depth <= 1 { LIBRARY_DIR } else { DEEP_DIR };
    tree.join(subdir).join(format!("{id}.json"))
}

/// The canonical launch directory of this process, recorded as a new session's
/// `repository`. Not UTF-8 is an error rather than a lossy record.
fn launch_repository() -> anyhow::Result<String> {
    let directory = std::env::current_dir()
        .context("finding launch directory")?
        .canonicalize()
        .context("resolving launch directory")?;
    directory
        .into_os_string()
        .into_string()
        .map_err(|directory| anyhow::anyhow!("launch directory is not valid UTF-8: {directory:?}"))
}

/// Split provider messages into turns, each starting at a user message. Messages
/// before the first user message form a turn of their own.
fn turn_ranges(messages: &[Message]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::User {
            if let Some(first) = start.replace(index) {
                ranges.push(first..index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(first) = start {
        ranges.push(first..messages.len());
    }
    ranges
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

/// Names of saved trees, least recently changed root first (`root.json` mtime). No
/// sessions directory means no sessions.
pub fn list(harness_dir: &Path) -> anyhow::Result<Vec<String>> {
    let dir = harness_dir.join(SESSIONS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // An entry without `library/root.json` is not a tree: a pre-RLM flat file
        // (`NotADirectory`), or a directory whose first save was interrupted before
        // the root file was renamed into place (`NotFound`).
        let root = file_path(&entry.path(), 0, ROOT_ID);
        let modified = match std::fs::metadata(&root).and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
        };
        sessions.push((modified, name));
    }
    sessions.sort();
    Ok(sessions.into_iter().map(|(_, name)| name).collect())
}

impl Session {
    /// A new root session at depth 0, in a tree named by its start time. The file is
    /// not created until the first `save`, so a launch with no messages leaves nothing
    /// behind.
    pub fn new(harness_dir: &Path) -> anyhow::Result<Self> {
        Ok(Session {
            sessions_dir: harness_dir.join(SESSIONS_DIR),
            tree: chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
            id: ROOT_ID.to_string(),
            turns: Vec::new(),
            local: LocalMetadata {
                repository: launch_repository()?,
                depth: 0,
                parent: None,
                turn_token_cache: Vec::new(),
            },
            revision: 0,
        })
    }

    /// A new branch in this session's tree for the `rlm` call `call_id`, one depth
    /// below this session and recording it as `parent`. Like `new`, the file is not
    /// created until the first `save`.
    pub fn branch(&self, call_id: &str) -> anyhow::Result<Self> {
        // Branch IDs use the encoded call-ID format, which also keeps them valid file
        // names distinct from `ROOT_ID`.
        ensure!(
            crate::tool_call_id::decode(call_id).is_some(),
            "invalid branch call ID: {call_id:?}"
        );
        Ok(Session {
            sessions_dir: self.sessions_dir.clone(),
            tree: self.tree.clone(),
            id: call_id.to_string(),
            turns: Vec::new(),
            local: LocalMetadata {
                repository: launch_repository()?,
                depth: self.local.depth + 1,
                parent: Some(self.id.clone()),
                turn_token_cache: Vec::new(),
            },
            revision: 0,
        })
    }

    /// Load the root of tree `name`, `library/root.json`. Tool calls left without
    /// results (the app was killed or crashed mid-turn) get `NOT_RUN` results, saved
    /// immediately, since the endpoint rejects every request while any are missing.
    /// Returns how many were added.
    pub fn resume(harness_dir: &Path, name: &str) -> anyhow::Result<(Self, usize)> {
        validate_name(name)?;
        let sessions_dir = harness_dir.join(SESSIONS_DIR);
        let path = file_path(&sessions_dir.join(name), 0, ROOT_ID);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: SessionFile =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let turns = turns_from_entries(file.messages)
            .with_context(|| format!("parsing {}", path.display()))?;
        // The root file's location fixes its identity; metadata claiming otherwise is
        // corrupt, and accepting it would move the file on the next save (`path`).
        ensure!(
            file.local.depth == 0 && file.local.parent.is_none(),
            "parsing {}: the root session must record depth 0 and no parent",
            path.display()
        );
        let mut session = Session {
            sessions_dir,
            tree: name.to_string(),
            id: ROOT_ID.to_string(),
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

    /// This session's file, derived from the current tree name, so it follows
    /// `/rename`.
    pub fn path(&self) -> PathBuf {
        file_path(
            &self.sessions_dir.join(&self.tree),
            self.local.depth,
            &self.id,
        )
    }

    /// This tree's `library/` directory (the root and depth 1 transcripts), derived
    /// from the current tree name, so it follows `/rename`.
    pub fn library_dir(&self) -> PathBuf {
        self.sessions_dir.join(&self.tree).join(LIBRARY_DIR)
    }

    /// The recorded depth: 0 for the root.
    pub fn depth(&self) -> u32 {
        self.local.depth
    }

    /// The name that identifies this session in failure records: the tree directory
    /// name for the root, the call ID for a branch. The App also shows it on screen for
    /// the root; a branch is shown by its caller-given `rlm` name instead.
    pub fn name(&self) -> &str {
        if self.local.depth == 0 {
            &self.tree
        } else {
            &self.id
        }
    }

    /// Every durable provider message in transcript order, including hidden turns.
    pub fn messages(&self) -> impl DoubleEndedIterator<Item = &Message> {
        self.turns.iter().flat_map(|turn| turn.messages.iter())
    }

    /// Provider messages selected by the context editor: the conversation requests send.
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

    /// `/rename`: rename the tree directory, or before the first save, the directory
    /// it will be created as. Every session file in the tree moves with it. An
    /// existing destination is never overwritten; the explicit check matters because
    /// a directory rename silently replaces an empty destination directory.
    pub fn rename(&mut self, name: &str) -> anyhow::Result<()> {
        validate_name(name)?;
        if name == self.tree {
            return Ok(());
        }
        let from = self.sessions_dir.join(&self.tree);
        let to = self.sessions_dir.join(name);
        if to.exists() {
            bail!("session {name} already exists");
        }
        if from.exists() {
            std::fs::rename(&from, &to)
                .with_context(|| format!("renaming {} to {}", from.display(), to.display()))?;
        }
        self.tree = name.to_string();
        Ok(())
    }

    /// Remove tool calls matching `matches`, with results, and save if
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
                    let remove = matches(call);
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

        let path = self.path();
        // The first save of a tree creates `sessions/<name>/` and its subdirectory.
        if let Some(dir) = path.parent() {
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
        let tmp = path.with_extension("json.tmp");
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        /// Isolate persisted sessions under a unique scratch directory.
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "harness-session-{}-{}-{}",
                std::process::id(),
                chrono::Local::now().timestamp_nanos_opt().unwrap(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }

        /// Use a fixed session name within this fixture for resume assertions.
        fn session(&self) -> Session {
            let mut session = Session::new(&self.0).unwrap();
            session.tree = "test".into();
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

    /// Borrow the flat provider-message view used by transcript and request tests.
    fn messages(session: &Session) -> Vec<&Message> {
        session.messages().collect()
    }

    /// Compare the provider-message state independently of local caches.
    fn state(session: &Session) -> Value {
        json!({"messages": messages(session)})
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
            serde_json::from_str(&std::fs::read_to_string(session.path()).unwrap()).unwrap();
        assert_eq!(
            saved["messages"][3],
            json!({"_harness_local":{"hidden":true}})
        );
        assert_eq!(session.visible_messages().len(), 1);
        assert_eq!(session.visible_messages()[0].content, "second");
        assert_eq!(
            json!(session.visible_messages()),
            json!([{"role":"user","content":"second"}])
        );

        let mut resumed = fixture.resume().unwrap().0;
        assert!(resumed.turns[0].hidden);
        assert!(!resumed.turns[1].hidden);
        resumed.toggle_hidden(0).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(resumed.path()).unwrap()).unwrap();
        assert!(
            saved["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry.get("_harness_local").is_none())
        );
    }

    /// Markers must follow exactly one derived turn; unmarked sessions stay fully visible.
    #[test]
    fn marker_validation_preserves_unmarked_sessions() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("message".into()).unwrap();
        assert!(!fixture.resume().unwrap().0.turns[0].hidden);
        // Hand-written files carry the required metadata so only the markers vary.
        let local = json!(session.local);

        std::fs::write(
            session.path(),
            json!({"messages":[
                {"role":"user","content":"message"},
                {"_harness_local":{"hidden":false}}
            ], "_harness_local": local})
            .to_string(),
        )
        .unwrap();
        let visible = fixture.resume().unwrap().0;
        assert!(!visible.turns[0].hidden);
        visible.save().unwrap();
        let canonical: Value =
            serde_json::from_str(&std::fs::read_to_string(visible.path()).unwrap()).unwrap();
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
            std::fs::write(
                session.path(),
                json!({"messages":messages, "_harness_local": local}).to_string(),
            )
            .unwrap();
            assert!(fixture.resume().is_err());
        }

        std::fs::write(
            session.path(),
            json!({"messages":[
                {"role":"user","content":"message"},
                {"_harness_local":{"hidden":"yes"}}
            ], "_harness_local": local})
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
        // Otherwise-valid metadata, so each file fails only for its unknown field.
        let local = json!(session.local);
        let mut unknown_local = local.clone();
        unknown_local["unknown"] = json!(true);
        for invalid in [
            json!({"messages": messages, "_harness_local": local, "injections": []}),
            json!({"messages": messages, "_harness_local": unknown_local}),
        ] {
            std::fs::write(session.path(), invalid.to_string()).unwrap();
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
            serde_json::from_str(&std::fs::read_to_string(session.path()).unwrap()).unwrap();
        assert_eq!(saved["_harness_local"]["turn_token_cache"][0]["total"], 42);
        let resumed = fixture.resume().unwrap().0;
        assert_eq!(resumed.local.turn_token_cache, [cache]);
        assert_eq!(json!(messages(&resumed)), saved["messages"]);
    }

    /// Failed submission saves preserve the in-memory conversation.
    #[test]
    fn failed_saves_preserve_memory() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.submit("earlier".into()).unwrap();
        session.submit("later".into()).unwrap();
        let current = messages(&session);
        assert_eq!(current[0].content, "earlier");
        assert_eq!(current[1].content, "later");
        let before = state(&session);
        assert_eq!(state(&fixture.resume().unwrap().0), before);
        let blocked = fixture.0.join("not-a-directory");
        std::fs::write(&blocked, "blocked").unwrap();
        session.sessions_dir = blocked.join("sessions");
        assert!(session.submit("new draft".into()).is_err());
        assert_eq!(state(&session), before);
        assert_eq!(state(&fixture.resume().unwrap().0), before);
    }
}
