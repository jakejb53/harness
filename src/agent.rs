//! One agent: a session and the tool-calling turn running in it.
//!
//! A turn starts with a user message and alternates between streaming a response and
//! handling ordered tool calls, with prompts at host operations. An `rlm` call waits in
//! the queue's order while the App runs its branch. The turn ends when a response has
//! no tool calls, on Esc, on a stream error, or, for a branch, on any error.
//!
//! Ownership boundary: the agent owns its session, turn state, tool set, and reported
//! usage, and every change to its turn and `Phase` happens here. It receives the App's
//! shared resources per call and returns outcomes the App acts on; display-only notes
//! and the failure log belong to the App.

use std::collections::VecDeque;

use anyhow::Context;
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::client::{self, StreamEvent};
use crate::config::{self, Settings};
use crate::grep;
use crate::host::{self, Mounts};
use crate::list;
use crate::patch;
use crate::read;
use crate::repl::{Repl, ReplEvent, Runtime};
use crate::root::{Access, Library, Root};
use crate::session::{FunctionCall, Message, NOT_RUN, Role, Session, ToolCall};
use crate::token_count::TokenTotal;
use crate::tool_call_id;
use crate::tool_reasoning;

/// Marks output cut short by Esc or exit, on its own line after the output so far.
pub const STOPPED: &str = "[stopped by user]";

/// `text` followed by `line` on its own line: a newline goes between them unless
/// `text` is empty or already ends with one.
pub fn append_line(mut text: String, line: &str) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text
}

/// The App's shared resources, owned by the App for its whole run and lent to each
/// agent call; agents never keep them.
pub struct Resources {
    pub settings: Settings,
    pub http: reqwest::Client,
    pub runtime: Runtime,
    pub patch_runtime: patch::Runtime,
    pub read_runtime: read::Runtime,
    pub grep_runtime: grep::Runtime,
    pub list_runtime: list::Runtime,
}

/// What an agent step reports for the App to act on.
pub enum Outcome {
    /// Nothing for the App to do.
    None,
    /// A REPL failure occurrence for the persistent failure log and its note.
    Failure(Failure),
    /// The response failed with this stream error. The reply stays in
    /// `Phase::Streaming`, so the App can take its text so far (`partial_content`)
    /// before stopping the turn (`stop_turn`): the root shows the error after the last
    /// message, and a branch fails.
    StreamError(String),
    /// An accepted `rlm` call (`call_id`) put this agent in `Phase::Delegating`; the
    /// App creates the branch with `task` as its user message and pushes it. The
    /// branch's answer comes back through `finish_delegation`.
    StartBranch { call_id: String, task: String },
}

/// Facts about one REPL failure. The running call keeps its phase; the App persists
/// the record under this agent's session name before showing the occurrence count
/// under the call.
pub struct Failure {
    pub call_id: String,
    pub kind: &'static str,
    pub message: String,
    pub code: String,
}

/// One agent: a session, the turn running in it, the tools it offers, and the usage
/// its latest replies reported. Its depth is its session's recorded `local.depth`.
pub struct Agent {
    session: Session,
    turn: Option<Turn>,
    /// Tool definitions sent with every request and listed by `/tools`; built at
    /// creation from this agent's depth and mounts (`mounts`) and fixed thereafter.
    /// A call naming any other tool gets `Unknown tool: <name>` (`offers`).
    tools: Vec<Value>,
    /// Content of the final response (one without tool calls) of the latest turn, set
    /// when that response ends and cleared when a turn starts or is stopped. A
    /// branch's answer to its parent's `rlm` call (`answer`), or the text it passes up
    /// with its marker when it stops or fails after that response (`partial_content`).
    final_content: Option<String>,
    /// The tree's library, for this agent's REPL `/library` mount and its direct tools'
    /// `root = "library"`, supplied by the App at creation at depth 2 and deeper only.
    library: Option<Library>,
    /// Context in use when the last reply finished (prompt + completion), as reported
    /// by the server. Unknown until a reply reports usage; stopped or failed replies
    /// report none, so the previous value stays.
    context_tokens: Option<u64>,
    /// Prompt tokens of the last reply, sent to the model as the next turn's FYI
    /// snapshot. Lags one turn behind; the first turn has no measurement yet.
    prompt_tokens: Option<u64>,
}

/// Fields are public so the App can draw them; the App only ever holds a shared
/// reference (`Agent::turn`), so all changes stay in the agent.
pub struct Turn {
    pub phase: Phase,
    /// Tool calls from the last response not yet handled, in order.
    pub queue: VecDeque<ToolCall>,
    /// Started by the turn's first call; dropping it aborts execution and its worker.
    pub repl: Option<Repl>,
}

pub enum Phase {
    /// Between steps: `advance` handles the next queued call, or sends the next
    /// request once the queue is empty.
    Idle,
    Streaming(Reply),
    /// Arguments validated; `advance` runs it.
    Ready(PendingCall),
    /// Direct Rust editing owns its worker until every committed change is reported.
    Patching {
        call: ToolCall,
        job: patch::Job,
    },
    /// Reads run independently of the REPL and use the same ordered tool queue.
    Reading {
        call: ToolCall,
        job: read::Job,
    },
    /// Grep owns a confined read worker until completion or cancellation.
    Searching {
        call: ToolCall,
        job: grep::Job,
    },
    /// Metadata listings share the ordered file-tool lifecycle without opening file contents.
    Listing {
        call: ToolCall,
        job: list::Job,
    },
    Running {
        call: PendingCall,
        output: String,
        prompt: Option<PendingPrompt>,
    },
    /// An accepted `rlm` call waits while its branch runs above this agent on the App's
    /// stack; the branch's answer is recorded as its result (`finish_delegation`).
    Delegating(ToolCall),
    /// Keep cleanup cancelable and the UI responsive while returning the worker.
    Finishing(JoinHandle<anyhow::Result<()>>),
}

pub struct PendingPrompt {
    pub operation: String,
    pub answer: oneshot::Sender<bool>,
    pub description: Option<ApprovalDescription>,
}

impl PendingPrompt {
    /// Cancel explanation work before allowing the suspended host operation to resume.
    pub fn respond(self, allow: bool) -> anyhow::Result<()> {
        drop(self.description);
        self.answer
            .send(allow)
            .map_err(|_| anyhow::anyhow!("couldn't answer approval: the REPL stopped waiting"))
    }
}

/// Owned by one approval, so resolving or abandoning it cannot update a later prompt.
pub struct ApprovalDescription {
    pub task: Option<JoinHandle<anyhow::Result<String>>>,
    pub text: String,
}

impl ApprovalDescription {
    /// Begin independently of the REPL driver; the approval answer stays immediately usable.
    fn start(
        http: reqwest::Client,
        settings: config::ApprovalDescriptionSettings,
        input: String,
    ) -> Self {
        Self {
            task: Some(tokio::spawn(async move {
                client::describe_approval(&http, &settings, &input).await
            })),
            text: "Generating description…".into(),
        }
    }

    /// Consume the completed request once; errors stay visible without blocking approval.
    pub fn complete(&mut self, result: anyhow::Result<String>) {
        self.task = None;
        self.text = match result {
            Ok(text) => format!("Description: {text}"),
            Err(error) => format!("Description unavailable: {error:#}"),
        };
    }
}

impl Drop for ApprovalDescription {
    /// Dropping a JoinHandle alone detaches its task, so explicitly cancel pending HTTP work.
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub struct PendingCall {
    pub call: ToolCall,
    pub code: String,
}

/// A response in progress. Everything received so far.
pub struct Reply {
    pub task: JoinHandle<()>,
    pub events: UnboundedReceiver<StreamEvent>,
    /// The message assembled from the deltas so far (`client::accumulate`), every
    /// field as the endpoint sent it.
    pub fields: Map<String, Value>,
}

pub enum TurnEvent {
    Stream(StreamEvent),
    Finished(anyhow::Result<()>),
    Repl(anyhow::Result<ReplEvent>),
    Patch(String),
    Read(String),
    Grep(String),
    List(String),
    Description(anyhow::Result<String>),
}

impl Agent {
    /// An idle agent for `session`, with no usage reported yet. `library` is the tree's
    /// library, given at depth 2 and deeper only. Its tool definitions are built here
    /// from its depth and from the same mounts (`mounts`) that decide which roots its
    /// calls may name, so the roots it offers and the roots it accepts cannot differ.
    pub fn new(session: Session, settings: &Settings, library: Option<Library>) -> Self {
        let mut agent = Self {
            session,
            turn: None,
            tools: Vec::new(),
            final_content: None,
            library,
            context_tokens: None,
            prompt_tokens: None,
        };
        agent.tools = client::tool_definitions(
            agent.session.depth(),
            settings.max_depth,
            &settings.repl_tool_description,
            agent.mounts(),
        );
        agent
    }

    /// The session the App draws, counts, and names.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Session access for the App's own session operations (submitting the user
    /// message, `/rename`, the token cache); turn state is never reachable this way.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The turn in progress, read-only, for drawing and status.
    pub fn turn(&self) -> Option<&Turn> {
        self.turn.as_ref()
    }

    /// The tool definitions this agent offers the model.
    pub fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// Prompt tokens the endpoint reported for this agent's latest reply, if known.
    pub fn prompt_tokens(&self) -> Option<u64> {
        self.prompt_tokens
    }

    /// Context tokens the endpoint reported for this agent's latest reply, if known.
    pub fn context_tokens(&self) -> Option<u64> {
        self.context_tokens
    }

    /// The content of the final response once the turn has ended with one (its REPL
    /// worker returned too); `None` while the turn runs, after a stop, and before any
    /// turn. For a branch, this is its parent's `rlm` result.
    pub fn answer(&self) -> Option<&str> {
        match self.turn {
            None => self.final_content.as_deref(),
            Some(_) => None,
        }
    }

    /// The response text a stopped or failed branch passes to its parent ahead of its
    /// marker line: the content streamed so far of the response in progress, or, once
    /// the final response has fully streamed, its complete content (while its worker is
    /// returned, or after returning it or saving the response failed), so that text is
    /// never dropped. Empty when no response text exists, such as while a call runs.
    /// Read it before `stop_turn`, which discards both.
    pub fn partial_content(&self) -> String {
        match self.turn.as_ref().map(|turn| &turn.phase) {
            Some(Phase::Streaming(reply)) => reply
                .fields
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            // `final_content` is `None` until the turn's final response ends, so in any
            // other phase of a running turn this is empty.
            _ => self.final_content.clone().unwrap_or_default(),
        }
    }

    /// Record `answer`, the branch's answer, as the result of the `rlm` call this agent
    /// is delegating, and leave the turn `Idle` so `advance` continues with the next
    /// call. The result is capped at this agent's output limit like any tool result.
    pub fn finish_delegation(&mut self, res: &Resources, answer: String) -> anyhow::Result<()> {
        // Checked before taking the phase, so any other phase stays as it was.
        let Some(turn) = self
            .turn
            .as_mut()
            .filter(|turn| matches!(turn.phase, Phase::Delegating(_)))
        else {
            anyhow::bail!("no rlm call is waiting for a subagent's answer");
        };
        let Phase::Delegating(call) = std::mem::replace(&mut turn.phase, Phase::Idle) else {
            unreachable!("the phase was checked above");
        };
        self.record_result(res, &call, answer)
    }

    /// Expose the single pending prompt to key routing, completion, and status rendering.
    pub fn pending_prompt(&self) -> Option<&PendingPrompt> {
        match &self.turn.as_ref()?.phase {
            Phase::Running { prompt, .. } => prompt.as_ref(),
            _ => None,
        }
    }

    /// Flip a completed turn's visibility. Reported usage described the old selection,
    /// so a successful change drops it.
    pub fn toggle_hidden(&mut self, turn_index: usize) -> anyhow::Result<()> {
        self.session.toggle_hidden(turn_index)?;
        invalidate_reported_usage(&mut self.context_tokens, &mut self.prompt_tokens);
        Ok(())
    }

    /// Open a turn once its user message is saved. Returns the pre-removal indices of
    /// the messages the per-turn cleanup removed, for the App's note positions.
    pub fn start_turn(&mut self) -> anyhow::Result<Vec<usize>> {
        self.final_content = None;
        // FYI/help are the only per-turn cleanup cases.
        let removed = self.session.remove_calls(is_cleanup)?;
        // Each turn opens with an approval-exempt synthetic FYI call made as if by the
        // model: the `FYI` tool at the root, `FYI()` in the REPL below it. `advance`
        // runs it, then sends the first request.
        let calls = fyi_calls(self.session.depth())?;
        let saved = self.session.push(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: Some(calls.clone()),
            ..Default::default()
        });
        // Queue the calls before the save result is checked: the message is in the
        // session either way, and an error exit must still record their results.
        self.turn = Some(Turn {
            phase: Phase::Idle,
            queue: calls.into(),
            repl: None,
        });
        saved?;
        Ok(removed)
    }

    /// Answer only the suspended host operation; the driver produces any denial result.
    pub fn approve(&mut self, allow: bool) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Running { prompt, .. } = &mut turn.phase else {
            return Ok(());
        };
        if let Some(prompt) = prompt.take() {
            prompt.respond(allow)?;
        }
        Ok(())
    }

    /// Apply one event from the active phase (`next_turn_event`). A finished worker
    /// ends the turn; a finished job records its call's result.
    pub fn on_event(&mut self, res: &Resources, event: TurnEvent) -> anyhow::Result<Outcome> {
        match event {
            TurnEvent::Stream(event) => return self.on_stream(event),
            TurnEvent::Finished(result) => {
                self.turn = None;
                result.context("ending REPL session")?;
            }
            TurnEvent::Repl(event) => return self.on_repl(res, event),
            TurnEvent::Patch(result) => {
                if let Some(turn) = self.turn.as_mut()
                    && let Phase::Patching { call, .. } =
                        std::mem::replace(&mut turn.phase, Phase::Idle)
                {
                    self.record_result(res, &call, result)?;
                }
            }
            TurnEvent::Read(result) | TurnEvent::Grep(result) | TurnEvent::List(result) => {
                if let Some(turn) = self.turn.as_mut()
                    && let Phase::Reading { call, .. }
                    | Phase::Searching { call, .. }
                    | Phase::Listing { call, .. } =
                        std::mem::replace(&mut turn.phase, Phase::Idle)
                {
                    self.record_result(res, &call, result)?;
                }
            }
            TurnEvent::Description(result) => {
                if let Some(Turn {
                    phase:
                        Phase::Running {
                            prompt: Some(prompt),
                            ..
                        },
                    ..
                }) = &mut self.turn
                    && let Some(description) = &mut prompt.description
                {
                    description.complete(result);
                }
            }
        }
        Ok(Outcome::None)
    }

    /// Move the turn forward until it waits on something: the stream, the
    /// user, the REPL, worker cleanup, or a branch. `prompt_tokens` is the FYI snapshot
    /// sent with a REPL call this step runs and reported by an `FYI` call it handles.
    /// Returns `Outcome::StartBranch` when an accepted `rlm` call starts delegating.
    pub async fn advance(
        &mut self,
        res: &Resources,
        prompt_tokens: TokenTotal,
    ) -> anyhow::Result<Outcome> {
        loop {
            let Some(turn) = self.turn.as_mut() else {
                return Ok(Outcome::None);
            };
            match std::mem::replace(&mut turn.phase, Phase::Idle) {
                Phase::Idle => match turn.queue.pop_front() {
                    Some(call) => match self.begin_call(res, call, prompt_tokens)? {
                        Outcome::None => {}
                        outcome => return Ok(outcome),
                    },
                    None => {
                        self.start_request(res);
                        return Ok(Outcome::None);
                    }
                },
                Phase::Ready(call) => self.run_call(res, call, prompt_tokens).await?,
                waiting => {
                    turn.phase = waiting;
                    return Ok(Outcome::None);
                }
            }
        }
    }

    /// Build each request from current settings and the authoritative session messages.
    fn start_request(&mut self, res: &Resources) {
        let messages = self
            .session
            .visible_messages()
            .iter()
            .map(|message| serde_json::json!(message))
            .collect();
        let (task, events) = client::start(
            &res.http,
            &res.settings.endpoint,
            res.settings.chat_prompt.as_deref(),
            &self.tools,
            messages,
        );
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = Phase::Streaming(Reply {
                task,
                events,
                fields: Map::new(),
            });
        }
    }

    /// Accumulate streamed deltas and usage. A stream error is returned with the reply
    /// still in place; the App takes its text and stops the turn (`Outcome::StreamError`).
    fn on_stream(&mut self, event: StreamEvent) -> anyhow::Result<Outcome> {
        let Some(Turn {
            phase: Phase::Streaming(reply),
            ..
        }) = &mut self.turn
        else {
            return Ok(Outcome::None);
        };
        // Take the events already queued too, so a fast stream costs one frame per
        // batch rather than one per delta.
        let mut event = Some(event);
        while let Some(current) = event.take() {
            match current {
                StreamEvent::Delta(delta) => accumulate_reply(&mut reply.fields, delta)?,
                StreamEvent::Usage { total, prompt } => {
                    self.context_tokens = Some(total);
                    if let Some(prompt) = prompt {
                        self.prompt_tokens = Some(prompt);
                    }
                }
                StreamEvent::Done { truncated } => {
                    self.end_stream(truncated)?;
                    return Ok(Outcome::None);
                }
                StreamEvent::Error(error) => return Ok(Outcome::StreamError(error)),
            }
            event = reply.events.try_recv().ok();
        }
        Ok(Outcome::None)
    }

    /// The response finished: queue its tool calls, or end the turn. A response cut
    /// off at the token limit is treated as interrupted.
    fn end_stream(&mut self, truncated: bool) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Streaming(reply) = std::mem::replace(&mut turn.phase, Phase::Idle) else {
            return Ok(());
        };
        let content = reply
            .fields
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let (calls, saved) = self.save_reply(reply, truncated);
        // Queue the calls before anything can fail, so an error exit still records a
        // result for each of them.
        if calls.is_empty() {
            // The final response: its content is the turn's answer (`answer`).
            self.final_content = Some(content);
            // Keep ownership in turn state until reset finishes, so Esc and exit
            // can abort the task and drop its REPL instead of detaching cleanup.
            if let Some(repl) = self.turn.as_mut().and_then(|turn| turn.repl.take()) {
                self.set_phase(Phase::Finishing(tokio::spawn(repl.finish())));
            } else {
                self.turn = None;
            }
        } else if let Some(turn) = self.turn.as_mut() {
            turn.queue = calls.into();
        }
        saved
    }

    /// Save the response as an assistant message. Returns its complete tool calls and
    /// the save result (the message is in the session either way; a failed save is
    /// retried by the next one). Provider call IDs are replaced before saving;
    /// other endpoint fields are retained. A call without an id or name is dropped, and when
    /// `interrupted`, so is a call whose arguments aren't valid JSON: it was still
    /// streaming and never ran, so nothing it did needs recording.
    fn save_reply(
        &mut self,
        reply: Reply,
        interrupted: bool,
    ) -> (Vec<ToolCall>, anyhow::Result<()>) {
        reply.task.abort();
        let mut fields = reply.fields;
        // The role is known, and `content` and `tool_calls` have typed homes; the rest
        // stays as streamed.
        fields.remove("role");
        let content = match fields.remove("content") {
            Some(Value::String(content)) => content,
            _ => String::new(),
        };
        let calls: Vec<ToolCall> = match fields.remove("tool_calls") {
            Some(Value::Array(calls)) => calls,
            _ => Vec::new(),
        }
        .into_iter()
        .filter_map(|mut call| {
            // `index` only orders fragments within the stream.
            call.as_object_mut()?.remove("index");
            let call: ToolCall = serde_json::from_value(call).ok()?;
            if interrupted && serde_json::from_str::<Value>(&call.function.arguments).is_err() {
                return None;
            }
            Some(call)
        })
        .collect();
        let message = Message {
            role: Role::Assistant,
            content,
            tool_calls: (!calls.is_empty()).then(|| calls.clone()),
            extra: fields,
            ..Default::default()
        };
        let saved = if message.is_empty() {
            Ok(())
        } else {
            self.session.push(message)
        };
        (calls, saved)
    }

    /// Validate and queue a call. Calls that can't run, and `FYI`, which answers at
    /// once, get their result recorded here, leaving the turn `Idle`. An accepted `rlm`
    /// call leaves the turn `Delegating` and returns `Outcome::StartBranch`.
    ///
    /// Ordering rule (here and in every handler below): a call's result is recorded,
    /// or the call is placed back in turn state, before anything that can fail, so an
    /// error exit still leaves a result for every call.
    fn begin_call(
        &mut self,
        res: &Resources,
        call: ToolCall,
        prompt_tokens: TokenTotal,
    ) -> anyhow::Result<Outcome> {
        // Only this agent's own tools run; any other name, including a tool another
        // depth has, is unknown here.
        if !self.offers(&call.function.name) {
            let error = format!("Unknown tool: {}", call.function.name);
            self.record_result(res, &call, error)?;
            return Ok(Outcome::None);
        }
        match call.function.name.as_str() {
            "FYI" => {
                // No parameters: the arguments must be a JSON object, whose members
                // are ignored as every tool ignores members it doesn't define.
                #[derive(serde::Deserialize)]
                struct Arguments {}
                let result = match serde_json::from_str::<Arguments>(&call.function.arguments) {
                    Ok(Arguments {}) => {
                        host::fyi_lines(Some(prompt_tokens), &res.settings.endpoint.model)
                    }
                    Err(error) => format!("Invalid FYI arguments: {error}"),
                };
                self.record_result(res, &call, result)?;
                Ok(Outcome::None)
            }
            "rlm" => self.begin_delegation(res, call),
            _ => {
                self.begin_execution(res, call)?;
                Ok(Outcome::None)
            }
        }
    }

    /// Accept an `rlm` call, or record that its arguments are invalid; an accepted
    /// call's `task` becomes the branch's user message verbatim.
    fn begin_delegation(&mut self, res: &Resources, call: ToolCall) -> anyhow::Result<Outcome> {
        #[derive(serde::Deserialize)]
        struct Arguments {
            task: String,
        }
        let task = match serde_json::from_str::<Arguments>(&call.function.arguments) {
            Ok(args) => args.task,
            Err(error) => {
                self.record_result(res, &call, format!("Invalid rlm arguments: {error}"))?;
                return Ok(Outcome::None);
            }
        };
        let call_id = call.id.clone();
        self.set_phase(Phase::Delegating(call));
        Ok(Outcome::StartBranch { call_id, task })
    }

    /// Whether `name` is one of this agent's tools (`tools`).
    fn offers(&self, name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool["function"]["name"].as_str() == Some(name))
    }

    /// Validate and queue a REPL or direct-tool call, under `begin_call`'s rules.
    fn begin_execution(&mut self, res: &Resources, call: ToolCall) -> anyhow::Result<()> {
        // Each direct tool validates its own arguments first, then its `root`.
        if call.function.name == "list" {
            let args = match serde_json::from_str::<list::Arguments>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    return self.record_result(
                        res,
                        &call,
                        format!("Invalid list arguments: {error}"),
                    );
                }
            };
            let access = match self.access("list", &call) {
                Ok(access) => access,
                Err(error) => return self.record_result(res, &call, error),
            };
            let job = res.list_runtime.start(args, access);
            self.set_phase(Phase::Listing { call, job });
            return Ok(());
        }
        if call.function.name == "grep" {
            let args = match serde_json::from_str::<grep::Arguments>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    return self.record_result(
                        res,
                        &call,
                        format!("Invalid grep arguments: {error}"),
                    );
                }
            };
            let access = match self.access("grep", &call) {
                Ok(access) => access,
                Err(error) => return self.record_result(res, &call, error),
            };
            let job = res.grep_runtime.start(args, access);
            self.set_phase(Phase::Searching { call, job });
            return Ok(());
        }
        if call.function.name == "read_file" {
            let args = match serde_json::from_str::<read::Arguments>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    return self.record_result(
                        res,
                        &call,
                        format!("Invalid read_file arguments: {error}"),
                    );
                }
            };
            let access = match self.access("read_file", &call) {
                Ok(access) => access,
                Err(error) => return self.record_result(res, &call, error),
            };
            let job = res.read_runtime.start(args, access);
            self.set_phase(Phase::Reading { call, job });
            return Ok(());
        }
        if call.function.name == "apply_patch" {
            let text = match patch_of(&call) {
                Ok(text) => text,
                Err(error) => return self.record_result(res, &call, error),
            };
            let access = match self.access("apply_patch", &call) {
                Ok(access) => access,
                Err(error) => return self.record_result(res, &call, error),
            };
            // The runtime refuses the library, which is read-only.
            return match res.patch_runtime.start(text, access) {
                Ok(job) => {
                    self.set_phase(Phase::Patching { call, job });
                    Ok(())
                }
                Err(error) => self.record_result(res, &call, format!("Not applied: {error}")),
            };
        }
        let code = match code_of(&call) {
            Ok(code) => code,
            Err(error) => return self.record_result(res, &call, error),
        };
        // `reasoning()` is a no-op the model is meant to call freely, so the call is
        // answered here without approval and without running: in the REPL, `reasoning`
        // is whatever the turn's earlier code bound it to, which could be anything.
        // Kept out of `is_exempt`, which also selects the calls the per-turn cleanup
        // removes.
        if tool_reasoning::is_reasoning_call(&code) {
            return self.record_result(res, &call, tool_reasoning::RESULT.to_string());
        }
        // Host operations enforce permissions individually. A synthetic call never
        // grants approval to unrelated code bundled alongside it or in arguments.
        self.set_phase(Phase::Ready(PendingCall { call, code }));
        Ok(())
    }

    /// The mounts this agent reaches, fixed by its depth and the library it was given:
    /// its REPL feeds (`Runtime::start`), its tool definitions (`new`), and the roots
    /// its direct-tool calls may name (`access`) all follow them.
    fn mounts(&self) -> Mounts {
        Mounts::at(self.session.depth(), self.library.is_some())
    }

    /// Resolve a direct-tool call's optional `root` (omitted or null means the
    /// repository) to what this agent reaches. Any other name this agent lacks, whether
    /// a real root or not, is the error result naming it.
    fn access(&self, tool: &str, call: &ToolCall) -> Result<Access, String> {
        #[derive(serde::Deserialize)]
        struct Arguments {
            root: Option<String>,
        }
        let Some(name) = serde_json::from_str::<Arguments>(&call.function.arguments)
            .map_err(|error| format!("Invalid {tool} arguments: {error}"))?
            .root
        else {
            return Ok(Access::Repository);
        };
        let unavailable = || format!("{tool}: root {name:?} is not available");
        match Root::parse(&name) {
            Some(Root::Repository) => Ok(Access::Repository),
            Some(Root::Store) if self.mounts().store => Ok(Access::Store),
            // The library capability is the agent's own, so holding it is reaching it.
            Some(Root::Library) => self
                .library
                .as_ref()
                .map(|library| Access::Library(library.dir.clone()))
                .ok_or_else(unavailable),
            _ => Err(unavailable()),
        }
    }

    /// Replace only the active phase, preserving queued calls and REPL ownership.
    fn set_phase(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = phase;
        }
    }

    /// Lazily start the turn's driver and enqueue code without awaiting execution.
    async fn run_call(
        &mut self,
        res: &Resources,
        call: PendingCall,
        prompt_tokens: TokenTotal,
    ) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        if turn.repl.is_none() {
            turn.repl = Some(res.runtime.start(
                self.session.depth(),
                self.library.as_ref().map(|library| library.mount.clone()),
            ));
        }
        let repl = turn.repl.as_mut().expect("started above");
        // Sending only queues a message; library loading and prompts happen on the
        // driver task after this call is restored to turn state.
        if let Err(error) = repl.send(&call.code, prompt_tokens).await {
            // Dropping the REPL kills it; the next call starts a fresh one.
            turn.repl = None;
            return self.record_result(res, &call.call, format!(
                "[REPL driver failed: {error:#}. This code was not run. REPL state was lost; the next call starts a fresh session and reloads the library.]"
            ));
        }
        turn.phase = Phase::Running {
            call,
            output: String::new(),
            prompt: None,
        };
        Ok(())
    }

    /// Keep results and host prompts attached to the active call; failure facts are
    /// returned for the App's persistent log while the call keeps running.
    fn on_repl(
        &mut self,
        res: &Resources,
        event: anyhow::Result<ReplEvent>,
    ) -> anyhow::Result<Outcome> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(Outcome::None);
        };
        let Phase::Running {
            call,
            output,
            prompt,
        } = &mut turn.phase
        else {
            return Ok(Outcome::None);
        };
        let (value, error) = match event {
            Ok(ReplEvent::Output(text)) => {
                output.push_str(&text);
                return Ok(Outcome::None);
            }
            Ok(ReplEvent::Prompt {
                operation,
                description_input,
                answer,
            }) => {
                anyhow::ensure!(prompt.is_none(), "REPL requested overlapping approvals");
                let description = res.settings.approval_description.as_ref().map(|settings| {
                    ApprovalDescription::start(
                        res.http.clone(),
                        settings.clone(),
                        description_input,
                    )
                });
                *prompt = Some(PendingPrompt {
                    operation,
                    answer,
                    description,
                });
                return Ok(Outcome::None);
            }
            Ok(ReplEvent::Failure {
                kind,
                message,
                code,
            }) => {
                return Ok(Outcome::Failure(Failure {
                    call_id: call.call.id.clone(),
                    kind,
                    message,
                    code,
                }));
            }
            Ok(ReplEvent::Done { value, error }) => (value, error),
            // Worker failures are reported by the driver. Closure here means the
            // driver itself failed, so discard it and make the state loss explicit.
            Err(error) => {
                turn.repl = None;
                (
                    None,
                    Some(format!(
                        "[REPL driver failed: {error:#}. REPL state was lost; the next call starts a fresh session and reloads the library.]"
                    )),
                )
            }
        };
        let Phase::Running {
            call, mut output, ..
        } = std::mem::replace(&mut turn.phase, Phase::Idle)
        else {
            return Ok(Outcome::None);
        };
        let extras: Vec<String> = [value, error].into_iter().flatten().collect();
        for extra in &extras {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(extra);
        }
        self.record_result(res, &call.call, output)?;
        Ok(Outcome::None)
    }

    /// Record a call's result, capped at the output limit.
    fn record_result(
        &mut self,
        res: &Resources,
        call: &ToolCall,
        result: String,
    ) -> anyhow::Result<()> {
        let result = if result.is_empty() {
            "(no output)".to_string()
        } else {
            truncate(result, res.settings.output_limit)
        };
        self.session.push(Message::tool(call.id.clone(), result))
    }

    /// End the turn now (Esc, `/exit`, app exit, a stream error, or any error that fails
    /// a branch). The partial reply is kept; running code is killed and its output so
    /// far recorded; every other complete call gets a `[not run]` result so the session
    /// stays valid, shown under its call.
    pub fn stop_turn(&mut self, res: &Resources) -> anyhow::Result<()> {
        // A stopped turn has no answer, even when it stops while returning its worker
        // after the final response.
        self.final_content = None;
        let Some(mut turn) = self.turn.take() else {
            return Ok(());
        };
        // A failed save doesn't stop the rest: each message is already in the
        // session, and a later save writes it.
        let mut first_error: Option<anyhow::Error> = None;
        let mut check = |result: anyhow::Result<()>| {
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        };
        let mut not_run = Vec::new();
        match std::mem::replace(&mut turn.phase, Phase::Idle) {
            Phase::Idle => {}
            Phase::Streaming(reply) => {
                let (calls, saved) = self.save_reply(reply, true);
                check(saved);
                not_run = calls;
            }
            Phase::Finishing(task) => {
                task.abort();
            }
            Phase::Ready(call) => not_run.push(call.call),
            // The App records a running branch's answer (`finish_delegation`) before
            // stopping its parent, so a call still delegating here never got a branch.
            Phase::Delegating(call) => not_run.push(call),
            Phase::Reading { call, job } => {
                drop(job);
                check(self.record_result(res, &call, STOPPED.into()));
            }
            Phase::Searching { call, job } => {
                drop(job);
                check(self.record_result(res, &call, STOPPED.into()));
            }
            Phase::Listing { call, job } => {
                drop(job);
                check(self.record_result(res, &call, STOPPED.into()));
            }
            Phase::Patching { call, mut job } => {
                // Join the writer after signaling cancellation: never save "stopped"
                // while a detached task could still mutate files or lose its receipt.
                check(self.record_result(res, &call, job.stop()));
            }
            Phase::Running {
                call,
                output,
                prompt,
            } => {
                if let Some(repl) = turn.repl.as_mut() {
                    repl.kill();
                }
                // Cancel execution before dropping the answer sender; otherwise
                // sandbox code could catch a canceled prompt and run more code.
                drop(prompt);
                check(self.record_result(res, &call.call, append_line(output, STOPPED)));
            }
        }
        not_run.extend(turn.queue.drain(..));
        for call in &not_run {
            check(self.record_result(res, call, NOT_RUN.to_string()));
        }
        drop(turn); // Kills the REPL, if any.

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Await the active phase without consuming state when terminal input wins the select.
    pub async fn next_turn_event(&mut self) -> TurnEvent {
        let Some(turn) = &mut self.turn else {
            return std::future::pending().await;
        };
        match &mut turn.phase {
            Phase::Streaming(reply) => {
                TurnEvent::Stream(reply.events.recv().await.unwrap_or_else(|| {
                    StreamEvent::Error("request task ended without a result".to_string())
                }))
            }
            Phase::Finishing(task) => TurnEvent::Finished(match task.await {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            }),
            Phase::Running { prompt, .. } => match turn.repl.as_mut() {
                Some(repl) => tokio::select! {
                    event = repl.next_event() => TurnEvent::Repl(event),
                    result = next_description(prompt) => TurnEvent::Description(result),
                },
                None => std::future::pending().await,
            },
            Phase::Patching { job, .. } => TurnEvent::Patch(job.result().await),
            Phase::Reading { job, .. } => TurnEvent::Read(job.result().await),
            Phase::Searching { job, .. } => TurnEvent::Grep(job.result().await),
            Phase::Listing { job, .. } => TurnEvent::List(job.result().await),
            // A delegating agent is not on top; the App awaits its branch instead.
            Phase::Idle | Phase::Ready(_) | Phase::Delegating(_) => std::future::pending().await,
        }
    }
}

/// Assign each streamed call its local identity once, before previews or persistence can use it.
fn accumulate_reply(
    fields: &mut Map<String, Value>,
    mut delta: Map<String, Value>,
) -> anyhow::Result<()> {
    if let Some(Value::Array(calls)) = delta.get_mut("tool_calls") {
        // The accumulator also merges repeated indices within a single chunk.
        let mut assigned_indices = Vec::new();
        for call in calls {
            let Some(call) = call.as_object_mut() else {
                continue;
            };
            if call
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                continue;
            }
            let index = call.get("index").cloned();
            let existing = index
                .as_ref()
                .and_then(|index| {
                    fields
                        .get("tool_calls")?
                        .as_array()?
                        .iter()
                        .find(|entry| entry.get("index") == Some(index))
                })
                .and_then(|entry| entry.get("id"))
                .and_then(Value::as_str);
            let assigned_here = index
                .as_ref()
                .is_some_and(|index| assigned_indices.contains(index));
            if assigned_here || existing.is_some_and(|id| !id.is_empty()) {
                // Later provider ID fragments must not append to the already assigned local ID.
                call.remove("id");
            } else {
                call.insert(
                    "id".into(),
                    Value::String(tool_call_id::generate(tool_call_id::NORMAL, &[])?),
                );
            }
            if let Some(index) = index {
                assigned_indices.push(index);
            }
        }
    }
    client::accumulate(fields, delta);
    Ok(())
}

/// The synthetic call opening each turn of an agent at `depth`: the `FYI` tool call at
/// the root, an `FYI()` REPL call below it. Marked with the synthetic ID kind so the
/// saved call stays available but its overlay row hides.
pub fn fyi_calls(depth: u32) -> anyhow::Result<Vec<ToolCall>> {
    let (name, arguments) = if depth == 0 {
        ("FYI", "{}")
    } else {
        ("REPL", r#"{"code": "FYI()"}"#)
    };
    Ok(vec![ToolCall {
        id: tool_call_id::generate(tool_call_id::SYNTHETIC, &[])?,
        kind: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: arguments.to_string(),
            extra: Map::new(),
        },
        extra: Map::new(),
    }])
}

/// Select the calls the per-turn cleanup removes: every `FYI` tool call, and every
/// REPL call made only of `FYI()`/`help()`, synthetic or the model's own.
fn is_cleanup(call: &ToolCall) -> bool {
    call.function.name == "FYI" || code_of(call).is_ok_and(|code| is_exempt(&code))
}

/// Select pure FYI/help calls for history cleanup. This selector does not grant
/// permissions: all actual host operations use the runtime's permission checks.
fn is_exempt(code: &str) -> bool {
    let mut statements = code
        .split([';', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .peekable();
    statements.peek().is_some() && statements.all(|s| matches!(s, "FYI()" | "help()"))
}

/// Reject malformed patch arguments before the editor can touch the filesystem.
pub fn patch_of(call: &ToolCall) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        patch: String,
    }
    serde_json::from_str::<Arguments>(&call.function.arguments)
        .map(|args| args.patch)
        .map_err(|error| format!("Invalid apply_patch arguments: {error}"))
}

/// The `code` argument of a REPL call, or the error to return to the model.
pub fn code_of(call: &ToolCall) -> Result<String, String> {
    if call.function.name != "REPL" {
        return Err(format!("Unknown tool: {}", call.function.name));
    }
    let arguments: Value = serde_json::from_str(&call.function.arguments)
        .map_err(|e| format!("Invalid arguments ({e}): {}", call.function.arguments))?;
    arguments["code"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "Invalid arguments: missing string `code`".to_string())
}

/// Cap `text` at `limit` bytes (on a character boundary), noting how much was cut.
fn truncate(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated: {} more bytes]",
        &text[..end],
        text.len() - end
    )
}

/// Provider usage describes the old selection and cannot override its new local total.
fn invalidate_reported_usage(context: &mut Option<u64>, prompt: &mut Option<u64>) {
    *context = None;
    *prompt = None;
}

/// Await only the current approval's request; cancellation never queues a stale result.
pub async fn next_description(prompt: &mut Option<PendingPrompt>) -> anyhow::Result<String> {
    let Some(task) = prompt
        .as_mut()
        .and_then(|prompt| prompt.description.as_mut())
        .and_then(|description| description.task.as_mut())
    else {
        return std::future::pending().await;
    };
    task.await.context("joining approval description request")?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Changing visibility invalidates both exact totals from the previous request.
    #[test]
    fn visibility_change_invalidates_reported_usage() {
        let mut context = Some(200);
        let mut prompt = Some(150);
        invalidate_reported_usage(&mut context, &mut prompt);
        assert_eq!(context, None);
        assert_eq!(prompt, None);
    }

    /// Provider IDs cannot introduce local markers; normalization preserves other call fields.
    #[test]
    fn provider_ids_and_results_use_normal_format() {
        let provider_ids = [
            "provider-call-id".to_owned(),
            tool_call_id::generate(tool_call_id::SYNTHETIC, &[]).unwrap(),
        ];
        let calls: Vec<ToolCall> = provider_ids
            .iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
            "id": id, "type": "function", "vendor": "preserved",
            "function": {"name": "REPL", "arguments": "{\"code\":\"1+1\"}", "vendor_function": 7}
        })).unwrap()
            })
            .collect();
        let before: Vec<_> = calls
            .iter()
            .map(|call| serde_json::to_value(call).unwrap())
            .collect();
        let mut fields = Map::new();
        accumulate_reply(
            &mut fields,
            serde_json::json!({"tool_calls": before})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let calls: Vec<ToolCall> = serde_json::from_value(fields["tool_calls"].clone()).unwrap();
        for ((call, original), previous_id) in calls.iter().zip(before).zip(provider_ids) {
            assert_ne!(call.id, previous_id);
            let metadata = tool_call_id::decode(&call.id).unwrap();
            assert_eq!(metadata.kind, tool_call_id::NORMAL);
            assert!(metadata.data.is_empty());
            let mut expected = original;
            expected["id"] = Value::String(call.id.clone());
            assert_eq!(serde_json::to_value(call).unwrap(), expected);
            let result = Message::tool(call.id.clone(), "2".into());
            let messages = vec![
                Message {
                    role: Role::Assistant,
                    tool_calls: Some(vec![call.clone()]),
                    ..Default::default()
                },
                result,
            ];
            let request = serde_json::json!(messages);
            assert_eq!(
                request[0]["tool_calls"][0]["id"],
                request[1]["tool_call_id"]
            );
        }
    }

    /// Provider ID fragments never change the preview identity or concatenate into the saved ID.
    #[test]
    fn streamed_call_ids_stay_stable() {
        let mut fields = Map::new();
        accumulate_reply(
            &mut fields,
            serde_json::json!({"tool_calls": [{
                "index": 0, "id": "provider-", "type": "function",
                "function": {"name": "REPL", "arguments": "{\"code\":\""}
            }]})
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        let first = fields["tool_calls"][0]["id"].as_str().unwrap().to_owned();
        accumulate_reply(
            &mut fields,
            serde_json::json!({"tool_calls": [{
                "index": 0, "id": "suffix", "function": {"arguments": "1+1\"}"}
            }, {
                "index": 1, "id": "second", "type": "function",
                "function": {"name": "REPL", "arguments": "{\"code\":\"2+2\"}"}
            }]})
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        assert_eq!(fields["tool_calls"][0]["id"], first);
        assert_ne!(fields["tool_calls"][1]["id"], first);
        assert_eq!(
            fields["tool_calls"][0]["function"]["arguments"],
            "{\"code\":\"1+1\"}"
        );
        for call in fields["tool_calls"].as_array().unwrap() {
            let metadata = tool_call_id::decode(call["id"].as_str().unwrap()).unwrap();
            assert_eq!(metadata.kind, tool_call_id::NORMAL);
        }
    }

    /// Several fragments for one index in a single chunk must still produce one 32-character ID.
    #[test]
    fn repeated_call_index_in_one_chunk_uses_one_id() {
        let mut fields = Map::new();
        accumulate_reply(&mut fields, serde_json::json!({"tool_calls": [
            {"index": 0, "id": "first", "type": "function", "function": {"name": "REPL", "arguments": "{\"code\":"}},
            {"index": 0, "id": "second", "function": {"arguments": "\"1\"}"}}
        ]}).as_object().unwrap().clone()).unwrap();
        let calls = fields["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        let id = calls[0]["id"].as_str().unwrap().to_owned();
        assert_eq!(id.len(), 32);
        assert_eq!(
            tool_call_id::decode(&id).unwrap().kind,
            tool_call_id::NORMAL
        );
        assert_eq!(calls[0]["function"]["arguments"], "{\"code\":\"1\"}");
        accumulate_reply(
            &mut fields,
            serde_json::json!({"tool_calls": [
                {"index": 0, "id": "third"}, {"index": 0, "id": "fourth"}
            ]})
            .as_object()
            .unwrap()
            .clone(),
        )
        .unwrap();
        assert_eq!(fields["tool_calls"][0]["id"], id);
    }

    /// Turn start marks only app-generated FYI, not model-authored calls.
    #[test]
    fn turn_start_calls_use_uniform_ids() {
        let calls = fyi_calls(1).unwrap();
        assert_eq!(calls.len(), 1);
        for (call, code) in calls.iter().zip(["FYI()"]) {
            assert_eq!(code_of(call).unwrap(), code);
            let metadata = tool_call_id::decode(&call.id).unwrap();
            assert_eq!(metadata.kind, tool_call_id::SYNTHETIC);
            assert!(metadata.data.is_empty());
        }
    }

    /// A slow request cannot delay yes/no; abandoning the prompt cancels its owned task too.
    #[tokio::test]
    async fn approval_answers_do_not_wait_for_description() {
        for answer in [Some(true), Some(false), None] {
            let (started, ready) = oneshot::channel();
            let task = tokio::spawn(async move {
                let _ = started.send(());
                std::future::pending::<anyhow::Result<String>>().await
            });
            let abort = task.abort_handle();
            ready.await.unwrap();
            let (sender, mut receiver) = oneshot::channel();
            let prompt = PendingPrompt {
                operation: "operation".into(),
                answer: sender,
                description: Some(ApprovalDescription {
                    task: Some(task),
                    text: "Generating description…".into(),
                }),
            };
            if let Some(answer) = answer {
                prompt.respond(answer).unwrap();
                assert_eq!(receiver.try_recv().unwrap(), answer);
            } else {
                drop(prompt);
                assert!(matches!(
                    receiver.try_recv(),
                    Err(oneshot::error::TryRecvError::Closed)
                ));
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !abort.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }
}
