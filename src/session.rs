//! Session messages are the durable conversation, before request-time tool reasoning.
//! Messages are appended, except that resume inserts results for
//! unanswered tool calls, and each turn removes earlier `FYI()`/`help()`-only calls
//! (`remove_calls`).
//! Partial replies are kept as-is, except that a tool call still
//! streaming when the reply ended is dropped. Assistant messages keep every field
//! the endpoint streamed, including ones the app doesn't know.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
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

#[derive(Serialize, Deserialize, Default)]
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

#[derive(Deserialize)]
struct SessionFile {
    messages: Vec<Message>,
}

/// Serialization twin of `SessionFile` that borrows the messages.
#[derive(Serialize)]
struct SessionFileRef<'a> {
    messages: &'a [Message],
}

pub struct Session {
    pub path: PathBuf,
    pub messages: Vec<Message>,
}

/// Session file name for `name`. Names must stay inside the sessions directory and
/// not collide with hidden or temp files: not empty, no `/`, no leading `.`.
fn file_name(name: &str) -> anyhow::Result<String> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid session name: {name:?}");
    }
    Ok(format!("{name}.json"))
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
            messages: Vec::new(),
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
        let mut session = Session {
            path,
            messages: file.messages,
        };
        let repaired = session.answer_unanswered_calls();
        if repaired > 0 {
            session.save()?;
        }
        Ok((session, repaired))
    }

    /// Insert a `NOT_RUN` result for each tool call without one, after the tool
    /// messages that follow its assistant message.
    fn answer_unanswered_calls(&mut self) -> usize {
        let mut added = 0;
        let mut i = 0;
        while i < self.messages.len() {
            let Some(calls) = &self.messages[i].tool_calls else {
                i += 1;
                continue;
            };
            let ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
            let mut end = i + 1;
            let mut answered = HashSet::new();
            while let Some(message) = self.messages.get(end)
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
            self.messages.splice(end..end, missing);
            i = end + inserted;
        }
        added
    }

    /// Append a model/tool message and immediately persist the conversation.
    pub fn push(&mut self, message: Message) -> anyhow::Result<()> {
        self.messages.push(message);
        self.save()
    }

    /// Persist an explicit user submission without retaining it in memory on save failure.
    pub fn submit(&mut self, text: String) -> anyhow::Result<()> {
        self.messages.push(Message::user(text));
        if let Err(error) = self.save() {
            self.messages.pop();
            return Err(error);
        }
        Ok(())
    }

    /// Attach completed local reads without a user message or any turn-start calls.
    pub fn inject(
        &mut self,
        prepared: Vec<crate::injection::PreparedInjection>,
    ) -> anyhow::Result<()> {
        let message_count = self.messages.len();
        for injection in prepared {
            let id = injection.call.id.clone();
            self.messages.push(Message {
                role: Role::Assistant,
                tool_calls: Some(vec![injection.call]),
                ..Default::default()
            });
            // User attachments preserve even empty contents and bypass the REPL output cap.
            self.messages.push(Message::tool(id, injection.content));
        }
        if let Err(error) = self.save() {
            self.messages.truncate(message_count);
            return Err(error);
        }
        Ok(())
    }

    /// Keep marked attachments exclusive and paired, even without a removal control.
    pub fn validate_injections(&self) -> anyhow::Result<()> {
        for (index, message) in self.messages.iter().enumerate() {
            let Some(calls) = &message.tool_calls else {
                continue;
            };
            for call in calls {
                if !crate::tool_call_id::decode(&call.id)
                    .is_some_and(|metadata| metadata.kind == crate::tool_call_id::INJECTED_FILE)
                {
                    continue;
                }
                // A marked call identifies one dedicated attachment, not a mixed message.
                let call_count = self
                    .messages
                    .iter()
                    .filter_map(|entry| entry.tool_calls.as_ref())
                    .flatten()
                    .filter(|entry| entry.id == call.id)
                    .count();
                if call_count != 1
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
                let results: Vec<_> = self
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry.tool_call_id.as_deref() == Some(call.id.as_str()))
                    .collect();
                if results.len() != 1
                    || results[0].0 != index + 1
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
        let mut emptied = vec![false; self.messages.len()];
        for (message, emptied) in self.messages.iter_mut().zip(&mut emptied) {
            let Some(calls) = &mut message.tool_calls else {
                continue;
            };
            calls.retain(|call| {
                // Only explicit instance removal may delete a user attachment.
                let injected = crate::tool_call_id::decode(&call.id)
                    .is_some_and(|metadata| metadata.kind == crate::tool_call_id::INJECTED_FILE);
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
        if removed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        let mut index = 0;
        let mut emptied = emptied.into_iter();
        self.messages.retain(|message| {
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
        self.save()?;
        Ok(removed)
    }

    /// Write via a synced temp file and rename so a crash or power loss never leaves a
    /// half-written file.
    fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(&SessionFileRef {
            messages: &self.messages,
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

    /// Compare the only persisted state: conversation messages.
    fn state(session: &Session) -> Value {
        json!({"messages": session.messages})
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
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[1].content, large);
        assert_eq!(session.messages[3].content, "");
        let first = &session.messages[0].tool_calls.as_ref().unwrap()[0].id;
        let second = &session.messages[2].tool_calls.as_ref().unwrap()[0].id;
        assert_ne!(first, second);
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert_eq!(saved, state(&session));
        assert_eq!(saved.as_object().unwrap().len(), 1);
        let (resumed, repaired) = fixture.resume().unwrap();
        assert_eq!(repaired, 0);
        assert_eq!(state(&resumed), saved);
        for enabled in [false, true] {
            assert_eq!(
                json!(crate::tool_reasoning::request_messages(
                    &resumed.messages,
                    enabled
                )),
                saved["messages"]
            );
        }
        session.submit("Review these".into()).unwrap();
        assert!(session.messages[4].role == Role::User);
        assert_eq!(json!(&session.messages[..4]), saved["messages"]);
    }

    /// Legacy metadata is ignored, and call IDs remain unchanged on resume.
    #[test]
    fn legacy_metadata_is_ignored_and_call_ids_are_preserved() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        let mut old = prepared("old contents");
        old.call.id = "legacy-id".into();
        let mut normal = prepared("normal contents");
        normal.call.id = crate::tool_call_id::generate(crate::tool_call_id::NORMAL, &[]).unwrap();
        session.inject(vec![old, normal]).unwrap();
        let messages = json!(session.messages);
        let legacy = json!({"messages": messages, "injections": "ignored even if malformed"});
        let text = legacy.to_string();
        std::fs::write(&session.path, &text).unwrap();
        let (mut resumed, repaired) = fixture.resume().unwrap();
        assert_eq!(repaired, 0);
        assert_eq!(std::fs::read_to_string(&session.path).unwrap(), text);
        resumed.submit("next".into()).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&session.path).unwrap()).unwrap();
        assert!(saved.get("injections").is_none());
        assert_eq!(json!(&resumed.messages[..4]), messages);
    }

    /// Even a broad automatic cleanup preserves instances carrying the injection marker.
    #[test]
    fn cleanup_preserves_marked_calls() {
        let fixture = Fixture::new();
        let mut session = fixture.session();
        session.inject(vec![prepared("file")]).unwrap();
        let id = session.messages[0].tool_calls.as_ref().unwrap()[0]
            .id
            .clone();
        let mut normal = prepared("environment");
        normal.call.id = "fyi-test".into();
        normal.call.function.arguments = json!({"code": "FYI()"}).to_string();
        session.inject(vec![normal]).unwrap();
        assert_eq!(session.remove_calls(|_| true).unwrap(), [2, 3]);
        assert_eq!(session.messages[0].tool_calls.as_ref().unwrap()[0].id, id);
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
        assert_eq!(session.messages[0].content, "earlier");
        assert_eq!(session.messages[7].content, "later");
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
            session.messages = serde_json::from_value(invalid["messages"].clone()).unwrap();
            assert!(
                session.validate_injections().is_err(),
                "accepted defect {defect}"
            );
        }
    }
}
