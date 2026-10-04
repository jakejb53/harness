//! Chat loop: input events, the tool-calling turn, and session persistence.
//!
//! A turn starts with a user message and alternates between streaming a response and
//! handling ordered tool calls, with prompts at host operations. It ends
//! when a response has no tool calls, on Esc, or on a stream error.
//!
//! The screen is drawn from state every frame (`build_blocks`): the session's
//! messages, the turn in progress, and display-only notes. Handlers only change
//! state; drawing is the loop's job.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui_textarea::TextArea;
use serde_json::{Map, Value};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::client::{self, StreamEvent};
use crate::commands::{self, Action};
use crate::config::{self, Settings, Theme};
use crate::failures::FailureLog;
use crate::grep;
use crate::injection::{self, PreparedInjection};
use crate::input::{self, InputAction};
use crate::list;
use crate::patch;
use crate::read;
use crate::repl::{Repl, ReplEvent, Runtime};
use crate::session::{FunctionCall, Message, NOT_RUN, Role, Session, ToolCall, reasoning_text};
use crate::tool_call_id;
use crate::tool_reasoning;
use crate::transcript::{Block, Key, View};
use crate::tui::{MessageOverlay, Tui};

const STOPPED: &str = "[stopped by user]";

/// Most rows either completion list shows at once.
const COMPLETION_ROWS: usize = 8;

/// Shared list entry; a missing command identifies a literal `@filename` completion.
struct Completion {
    spelling: String,
    command: Option<&'static commands::Command>,
}

/// Validate persistent failure history and start workers before taking over the terminal.
pub async fn run(settings: Settings, session: Session) -> anyhow::Result<()> {
    let failures = FailureLog::load(&settings.dir)?;
    let runtime = Runtime::new(&settings).await?;
    let patch_runtime = patch::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        settings.permission == config::Permission::ReadWrite,
    )?;
    let read_runtime = read::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        settings.permission != config::Permission::None,
    )?;
    let grep_runtime = grep::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        settings.permission != config::Permission::None,
    )?;
    let list_runtime = list::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        settings.permission != config::Permission::None,
    )?;
    let mut tui = Tui::enter()?;
    let textarea = input::new_textarea(&settings.theme);
    let mut app = App {
        settings,
        runtime,
        patch_runtime,
        read_runtime,
        grep_runtime,
        list_runtime,
        failures,
        session,
        http: reqwest::Client::new(),
        textarea,
        view: View::default(),
        notes: Vec::new(),
        turn: None,
        notice: None,
        context_tokens: None,
        prompt_tokens: None,
        signaled: false,
        completion: 0,
        completion_dismissed: None,
        file_completions: Vec::new(),
        file_completion_context: None,
        repo: std::env::current_dir().context("finding launch directory")?,
        loading: None,
        message_scroll: usize::MAX,
        follow_messages: false,
        overlay_gesture: false,
    };
    let result = app.run(&mut tui).await;
    app.runtime.close().await;
    let exited = tui.exit();
    // After a signal the terminal may be gone, so restoring it can fail harmlessly.
    if !app.signaled {
        exited?;
    }
    result
}

struct App {
    settings: Settings,
    runtime: Runtime,
    patch_runtime: patch::Runtime,
    read_runtime: read::Runtime,
    grep_runtime: grep::Runtime,
    list_runtime: list::Runtime,
    failures: FailureLog,
    session: Session,
    http: reqwest::Client,
    textarea: TextArea<'static>,
    view: View,
    /// Display-only lines (failure counts, errors), never saved in the session.
    notes: Vec<Note>,
    turn: Option<Turn>,
    /// UI-only message shown in the status line until the next key press.
    notice: Option<String>,
    /// Context in use when the last reply finished (prompt + completion), as reported
    /// by the server. Unknown until a reply reports usage; stopped or failed replies
    /// report none, so the previous value stays.
    context_tokens: Option<u64>,
    /// Prompt tokens of the last reply, sent to the model as the next turn's FYI
    /// snapshot. Lags one turn behind; the first turn has no measurement yet.
    prompt_tokens: Option<u64>,
    /// Exiting because of a signal: the terminal may be gone, so a failure to
    /// restore it is ignored.
    signaled: bool,
    /// Highlighted row of the command completion list. Back to the first match
    /// whenever the input changes.
    completion: usize,
    /// Input text for which Esc closed the completion list; it stays closed until
    /// the input changes.
    completion_dismissed: Option<String>,
    /// Directory-derived candidates refresh on editor changes, never during transcript redraws.
    file_completions: Vec<String>,
    file_completion_context: Option<(usize, String)>,
    /// User paths resolve from launch, independently of the sandbox's working directory.
    repo: PathBuf,
    /// No session changes occur until every explicit attachment has loaded successfully.
    loading: Option<PendingInjection>,
    /// First visible display position, held while idle or after manual scrolling.
    message_scroll: usize,
    /// Follow appended positions only until the user scrolls during this turn.
    follow_messages: bool,
    /// A gesture touching the overlay must not select or copy the transcript underneath it.
    overlay_gesture: bool,
}

/// Owns one local attachment batch; dropping it prevents late reads from changing history.
struct PendingInjection {
    text: String,
    task: JoinHandle<anyhow::Result<Vec<PreparedInjection>>>,
    canceled: Arc<AtomicBool>,
}

impl PendingInjection {
    /// Keep potentially large file reads off the event loop without granting model access.
    fn start(text: String, paths: Vec<String>, root: PathBuf) -> Self {
        let canceled = Arc::new(AtomicBool::new(false));
        let signal = canceled.clone();
        let task = tokio::task::spawn_blocking(move || injection::load(paths, root, signal));
        Self {
            text,
            task,
            canceled,
        }
    }
}

impl Drop for PendingInjection {
    /// Blocking reads cooperate between chunks; cancellation never commits a late result.
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Relaxed);
        self.task.abort();
    }
}

struct Turn {
    phase: Phase,
    /// Tool calls from the last response not yet handled, in order.
    queue: VecDeque<ToolCall>,
    /// Started by the turn's first call; dropping it aborts execution and its worker.
    repl: Option<Repl>,
}

enum Phase {
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
    /// Keep cleanup cancelable and the UI responsive while returning the worker.
    Finishing(JoinHandle<anyhow::Result<()>>),
}

struct PendingPrompt {
    operation: String,
    answer: oneshot::Sender<bool>,
    description: Option<ApprovalDescription>,
}

impl PendingPrompt {
    /// Cancel explanation work before allowing the suspended host operation to resume.
    fn respond(self, allow: bool) -> anyhow::Result<()> {
        drop(self.description);
        self.answer
            .send(allow)
            .map_err(|_| anyhow::anyhow!("couldn't answer approval: the REPL stopped waiting"))
    }
}

/// Owned by one approval, so resolving or abandoning it cannot update a later prompt.
struct ApprovalDescription {
    task: Option<JoinHandle<anyhow::Result<String>>>,
    text: String,
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
    fn complete(&mut self, result: anyhow::Result<String>) {
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

struct PendingCall {
    call: ToolCall,
    code: String,
}

/// A response in progress. Everything received so far.
struct Reply {
    task: JoinHandle<()>,
    events: UnboundedReceiver<StreamEvent>,
    /// The message assembled from the deltas so far (`client::accumulate`), every
    /// field as the endpoint sent it.
    fields: Map<String, Value>,
}

/// Display-only transcript output, never saved or sent to the model.
struct Note {
    anchor: Anchor,
    /// Tool names render above the body with the tool-result style; ordinary notes have no heading.
    heading: Option<String>,
    text: String,
    style: Style,
}

/// Where a note is shown. A note whose anchor is removed disappears with it.
enum Anchor {
    /// Under the call's code, above its result (failure occurrence notes).
    Call(String),
    /// After the first `n` messages (errors and local command output). Adjusted when earlier
    /// messages are removed.
    After(usize),
}

enum Step {
    Signal,
    Input(Option<io::Result<Event>>),
    Turn(TurnEvent),
    Injected(anyhow::Result<Vec<PreparedInjection>>),
}

enum TurnEvent {
    Stream(StreamEvent),
    Finished(anyhow::Result<()>),
    Repl(anyhow::Result<ReplEvent>),
    Patch(String),
    Read(String),
    Grep(String),
    List(String),
    Description(anyhow::Result<String>),
}

enum Flow {
    Continue,
    Exit,
}

impl App {
    async fn run(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let result = self.run_loop(tui).await;
        // Exiting mid-turn, including on an error or a signal, keeps the partial
        // reply and records results for its tool calls.
        let stopped = self.stop_turn(None);
        result.and(stopped)
    }

    async fn run_loop(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let mut input = spawn_input_reader();
        // Closing the terminal (SIGHUP) or a kill would otherwise skip cleanup,
        // orphaning the REPL and leaving tool calls without results. In raw mode
        // Ctrl+C is a key press, so SIGINT only arrives from outside.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;

        loop {
            // Preserve the existing fail-fast check for marked attachment pairs.
            self.session.validate_injections()?;
            let messages = message_overlay_entries(&self.session.messages)?;
            let footer = self.footer();
            let blocks = build_blocks(
                self.settings.chat_prompt.as_deref(),
                &self.session.messages,
                &self.notes,
                self.turn.as_ref(),
                self.settings.tool_reasoning,
                &self.settings.theme,
            );
            tui.draw(
                &blocks,
                &mut self.view,
                &self.textarea,
                &footer,
                &self.settings.theme,
                MessageOverlay {
                    entries: &messages,
                    scroll: if self.follow_messages {
                        usize::MAX
                    } else {
                        self.message_scroll
                    },
                },
            )?;
            self.message_scroll = tui.message_scroll();
            // Capture the last followed position when a turn ends; idle changes keep it.
            if self.turn.is_none() {
                self.follow_messages = false;
            }
            // Biased toward input so keys (Esc above all) are handled before more
            // output from a fast stream.
            let step = tokio::select! {
                biased;
                _ = hangup.recv() => Step::Signal,
                _ = terminate.recv() => Step::Signal,
                _ = interrupt.recv() => Step::Signal,
                event = input.recv() => Step::Input(event),
                event = next_turn_event(&mut self.turn) => Step::Turn(event),
                result = next_injection(&mut self.loading) => Step::Injected(result),
            };
            match step {
                Step::Injected(result) => self.finish_injection(result)?,
                Step::Signal => {
                    self.signaled = true;
                    return Ok(());
                }
                Step::Input(None) => return Ok(()),
                Step::Input(Some(event)) => {
                    if let Flow::Exit = self.on_input(tui, event?)? {
                        return Ok(());
                    }
                }
                Step::Turn(TurnEvent::Stream(event)) => self.on_stream(event)?,
                Step::Turn(TurnEvent::Finished(result)) => {
                    self.turn = None;
                    result.context("ending REPL session")?;
                }
                Step::Turn(TurnEvent::Repl(event)) => self.on_repl(event)?,
                Step::Turn(TurnEvent::Patch(result)) => {
                    if let Some(turn) = self.turn.as_mut()
                        && let Phase::Patching { call, .. } =
                            std::mem::replace(&mut turn.phase, Phase::Idle)
                    {
                        self.record_result(&call, result)?;
                    }
                }
                Step::Turn(
                    TurnEvent::Read(result) | TurnEvent::Grep(result) | TurnEvent::List(result),
                ) => {
                    if let Some(turn) = self.turn.as_mut()
                        && let Phase::Reading { call, .. }
                        | Phase::Searching { call, .. }
                        | Phase::Listing { call, .. } =
                            std::mem::replace(&mut turn.phase, Phase::Idle)
                    {
                        self.record_result(&call, result)?;
                    }
                }
                Step::Turn(TurnEvent::Description(result)) => {
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
            self.advance().await?;
        }
    }

    /// Move the turn forward until it waits on something: the stream, the
    /// user, the REPL, or worker cleanup.
    async fn advance(&mut self) -> anyhow::Result<()> {
        loop {
            let Some(turn) = self.turn.as_mut() else {
                return Ok(());
            };
            match std::mem::replace(&mut turn.phase, Phase::Idle) {
                Phase::Idle => match turn.queue.pop_front() {
                    Some(call) => self.begin_call(call)?,
                    None => {
                        self.start_request();
                        return Ok(());
                    }
                },
                Phase::Ready(call) => self.run_call(call).await?,
                waiting => {
                    turn.phase = waiting;
                    return Ok(());
                }
            }
        }
    }

    /// Build each request from current settings and the authoritative session messages.
    fn start_request(&mut self) {
        let (task, events) = client::start(
            &self.http,
            &self.settings.endpoint,
            self.settings.chat_prompt.as_deref(),
            &self.settings.repl_tool_description,
            tool_reasoning::request_messages(&self.session.messages, self.settings.tool_reasoning),
        );
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = Phase::Streaming(Reply {
                task,
                events,
                fields: Map::new(),
            });
        }
    }

    fn on_stream(&mut self, event: StreamEvent) -> anyhow::Result<()> {
        let Some(Turn {
            phase: Phase::Streaming(reply),
            ..
        }) = &mut self.turn
        else {
            return Ok(());
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
                StreamEvent::Done { truncated } => return self.end_stream(truncated),
                StreamEvent::Error(error) => return self.stop_turn(Some(error)),
            }
            event = reply.events.try_recv().ok();
        }
        Ok(())
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
        let (calls, saved) = self.save_reply(reply, truncated);
        // Queue the calls before anything can fail, so an error exit still records a
        // result for each of them.
        if calls.is_empty() {
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

    /// Validate and queue a call. Calls that can't run
    /// get their result recorded here, leaving the turn `Idle`.
    ///
    /// Ordering rule (here and in every handler below): a call's result is recorded,
    /// or the call is placed back in turn state, before anything that can fail, so an
    /// error exit still leaves a result for every call.
    fn begin_call(&mut self, call: ToolCall) -> anyhow::Result<()> {
        if call.function.name == "list" {
            return match serde_json::from_str::<list::Arguments>(&call.function.arguments) {
                Ok(args) => {
                    let job = self.list_runtime.start(args);
                    self.set_phase(Phase::Listing { call, job });
                    Ok(())
                }
                Err(error) => self.record_result(&call, format!("Invalid list arguments: {error}")),
            };
        }
        if call.function.name == "grep" {
            return match serde_json::from_str::<grep::Arguments>(&call.function.arguments) {
                Ok(args) => {
                    let job = self.grep_runtime.start(args);
                    self.set_phase(Phase::Searching { call, job });
                    Ok(())
                }
                Err(error) => self.record_result(&call, format!("Invalid grep arguments: {error}")),
            };
        }
        if call.function.name == "read_file" {
            let args = match serde_json::from_str::<read::Arguments>(&call.function.arguments) {
                Ok(args) => args,
                Err(error) => {
                    return self
                        .record_result(&call, format!("Invalid read_file arguments: {error}"));
                }
            };
            let job = self.read_runtime.start(args);
            self.set_phase(Phase::Reading { call, job });
            return Ok(());
        }
        if call.function.name == "apply_patch" {
            let text = match patch_of(&call) {
                Ok(text) => text,
                Err(error) => return self.record_result(&call, error),
            };
            return match self.patch_runtime.start(text) {
                Ok(job) => {
                    self.set_phase(Phase::Patching { call, job });
                    Ok(())
                }
                Err(error) => self.record_result(&call, format!("Not applied: {error}")),
            };
        }
        let code = match code_of(&call) {
            Ok(code) => code,
            Err(error) => return self.record_result(&call, error),
        };
        // `reasoning()` is a no-op the model is meant to call freely, so the call is
        // answered here without approval and without running: in the REPL, `reasoning`
        // is whatever the turn's earlier code bound it to, which could be anything.
        // Kept out of `is_exempt`, which also selects the calls the per-turn cleanup
        // removes.
        if tool_reasoning::is_reasoning_call(&code) {
            return self.record_result(&call, tool_reasoning::RESULT.to_string());
        }
        // Host operations enforce permissions individually. A synthetic call never
        // grants approval to unrelated code bundled alongside it or in arguments.
        self.set_phase(Phase::Ready(PendingCall { call, code }));
        Ok(())
    }

    /// Replace only the active phase, preserving queued calls and REPL ownership.
    fn set_phase(&mut self, phase: Phase) {
        if let Some(turn) = self.turn.as_mut() {
            turn.phase = phase;
        }
    }

    /// Attach a display-only note without changing the messages sent to the model.
    fn note(&mut self, anchor: Anchor, text: String, style: Style) {
        self.notes.push(Note {
            anchor,
            heading: None,
            text,
            style,
        });
    }

    /// Answer only the suspended host operation; the driver produces any denial result.
    fn approve(&mut self, allow: bool) -> anyhow::Result<()> {
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

    /// Lazily start the turn's driver and enqueue code without awaiting execution.
    async fn run_call(&mut self, call: PendingCall) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        if turn.repl.is_none() {
            turn.repl = Some(self.runtime.start());
        }
        let repl = turn.repl.as_mut().expect("started above");
        // Sending only queues a message; library loading and prompts happen on the
        // driver task after this call is restored to turn state.
        if let Err(error) = repl.send(&call.code, self.prompt_tokens).await {
            // Dropping the REPL kills it; the next call starts a fresh one.
            turn.repl = None;
            return self.record_result(&call.call, format!(
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

    /// Keep results, host prompts, and persistent failure facts attached to the active call.
    fn on_repl(&mut self, event: anyhow::Result<ReplEvent>) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Phase::Running {
            call,
            output,
            prompt,
        } = &mut turn.phase
        else {
            return Ok(());
        };
        let (value, error) = match event {
            Ok(ReplEvent::Output(text)) => {
                output.push_str(&text);
                return Ok(());
            }
            Ok(ReplEvent::Prompt {
                operation,
                description_input,
                answer,
            }) => {
                anyhow::ensure!(prompt.is_none(), "REPL requested overlapping approvals");
                let description = self.settings.approval_description.as_ref().map(|settings| {
                    ApprovalDescription::start(
                        self.http.clone(),
                        settings.clone(),
                        description_input,
                    )
                });
                *prompt = Some(PendingPrompt {
                    operation,
                    answer,
                    description,
                });
                return Ok(());
            }
            Ok(ReplEvent::Failure {
                kind,
                message,
                code,
            }) => {
                let anchor = Anchor::Call(call.call.id.clone());
                let session = self
                    .session
                    .path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .context("session path has no UTF-8 session name")?;
                // Persist before displaying the count; a failed append must not
                // claim a durable occurrence or lose ownership of the running call.
                let count = self.failures.record(session, kind, &message, &code)?;
                let first_line = message.lines().next().unwrap_or("");
                self.note(
                    anchor,
                    format!("{kind}: {first_line} (seen {count} times)"),
                    self.settings.theme.warning,
                );
                return Ok(());
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
            return Ok(());
        };
        let extras: Vec<String> = [value, error].into_iter().flatten().collect();
        for extra in &extras {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(extra);
        }
        self.record_result(&call.call, output)
    }

    /// Record a call's result, capped at the output limit.
    fn record_result(&mut self, call: &ToolCall, result: String) -> anyhow::Result<()> {
        let result = if result.is_empty() {
            "(no output)".to_string()
        } else {
            truncate(result, self.settings.output_limit)
        };
        self.session.push(Message::tool(call.id.clone(), result))
    }

    /// End the turn now (Esc, `/exit`, a stream error, or app exit). The partial reply
    /// is kept; running code is killed and its output so far recorded; every other
    /// complete call gets a `[not run]` result so the session stays valid, shown under
    /// its call. `error` (a stream error) is shown as a note.
    fn stop_turn(&mut self, error: Option<String>) -> anyhow::Result<()> {
        // Pre-submission cancellation preserves the draft and has no session effects.
        self.loading = None;
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
            Phase::Reading { call, job } => {
                drop(job);
                check(self.record_result(&call, STOPPED.into()));
            }
            Phase::Searching { call, job } => {
                drop(job);
                check(self.record_result(&call, STOPPED.into()));
            }
            Phase::Listing { call, job } => {
                drop(job);
                check(self.record_result(&call, STOPPED.into()));
            }
            Phase::Patching { call, mut job } => {
                // Join the writer after signaling cancellation: never save "stopped"
                // while a detached task could still mutate files or lose its receipt.
                check(self.record_result(&call, job.stop()));
            }
            Phase::Running {
                call,
                mut output,
                prompt,
            } => {
                if let Some(repl) = turn.repl.as_mut() {
                    repl.kill();
                }
                // Cancel execution before dropping the answer sender; otherwise
                // sandbox code could catch a canceled prompt and run more code.
                drop(prompt);
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(STOPPED);
                check(self.record_result(&call.call, output));
            }
        }
        not_run.extend(turn.queue.drain(..));
        for call in &not_run {
            check(self.record_result(call, NOT_RUN.to_string()));
        }
        drop(turn); // Kills the REPL, if any.

        if let Some(error) = error {
            let end = Anchor::After(self.session.messages.len());
            self.note(end, format!("error: {error}"), self.settings.theme.error);
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Route terminal events, restricting input while a host operation awaits approval.
    fn on_input(&mut self, tui: &mut Tui, event: Event) -> anyhow::Result<Flow> {
        if let Event::Mouse(mouse) = &event
            && self.message_mouse(tui, *mouse)
        {
            return Ok(Flow::Continue);
        }
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.notice = None;
                self.view.clear_selection();
                // Scroll keys work in every state, including at an approval prompt.
                let page = match key.code {
                    KeyCode::PageUp => Some(-1),
                    KeyCode::PageDown => Some(1),
                    _ => None,
                };
                if let Some(direction) = page {
                    self.view.page(direction);
                    return Ok(Flow::Continue);
                }
                // Attachment loading holds the draft until all reads finish.
                if self.loading.is_some() {
                    if key.code == KeyCode::Esc {
                        self.stop_turn(None)?;
                    }
                    return Ok(Flow::Continue);
                }
                // While a call awaits approval, only y/n, Esc, and Shift+Tab act.
                if self.pending_prompt().is_some() {
                    match key.code {
                        KeyCode::Char('y') => self.approve(true)?,
                        KeyCode::Char('n') => self.approve(false)?,
                        KeyCode::Esc => self.stop_turn(None)?,
                        KeyCode::BackTab => self.cycle_mode(),
                        _ => {}
                    }
                    return Ok(Flow::Continue);
                }
                // While the completion list shows, it takes the keys that act on it.
                let completions = self.completions();
                if !completions.is_empty() && key.modifiers.is_empty() {
                    let count = completions.len();
                    let selected = self.completion.min(count - 1);
                    match key.code {
                        KeyCode::Up => {
                            self.completion = (selected + count - 1) % count;
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Down => {
                            self.completion = (selected + 1) % count;
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Tab => {
                            let completion = &completions[selected];
                            if completion.command.is_some() {
                                self.textarea.clear();
                                self.textarea.insert_str(&completion.spelling);
                            } else {
                                // File spellings always begin with the attachment marker.
                                input::complete_file(&mut self.textarea, &completion.spelling[1..]);
                            }
                            self.input_changed();
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Enter if completions[selected].command.is_some() => {
                            let completion = &completions[selected];
                            let command = completion.command.expect("guarded command completion");
                            if !command.takes_argument {
                                return self.run_command(command.action, "");
                            }
                            // Can't run without its argument: complete it, followed by a
                            // space (which closes the list), ready for the argument.
                            self.textarea.clear();
                            self.textarea
                                .insert_str(format!("{} ", completion.spelling));
                            self.input_changed();
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Esc => {
                            self.completion_dismissed = Some(self.textarea.lines().join("\n"));
                            return Ok(Flow::Continue);
                        }
                        _ => {}
                    }
                }
                // No filename match means Tab leaves the literal path untouched.
                if key.code == KeyCode::Tab && input::file_prefix(&self.textarea).is_some() {
                    return Ok(Flow::Continue);
                }
                let before = self.textarea.lines().to_vec();
                let before_cursor = self.textarea.cursor();
                let action = input::handle_key(&mut self.textarea, key);
                if self.textarea.lines() != before.as_slice() {
                    self.input_changed();
                } else if self.textarea.cursor() != before_cursor {
                    self.refresh_file_completions();
                }
                match action {
                    InputAction::Submit(text) => return self.submit(text),
                    InputAction::Stop => self.stop_turn(None)?,
                    InputAction::CycleMode => self.cycle_mode(),
                    InputAction::None => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.view.scroll(-1),
                MouseEventKind::ScrollDown => self.view.scroll(1),
                MouseEventKind::Down(MouseButton::Left) => self.view.press(mouse.column, mouse.row),
                MouseEventKind::Drag(MouseButton::Left) => self.view.drag(mouse.column, mouse.row),
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(text) = self.view.release()
                        && let Err(error) = tui.copy(&text)
                    {
                        self.notice = Some(format!("couldn't copy: {error}"));
                    }
                }
                _ => {}
            },
            Event::Paste(text) if self.pending_prompt().is_none() && self.loading.is_none() => {
                input::paste(&mut self.textarea, &text);
                self.input_changed();
            }
            // A resize needs nothing here: the next frame lays out at the new size.
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Consume overlay gestures before transcript selection, even when a drag leaves the box.
    fn message_mouse(&mut self, tui: &Tui, mouse: MouseEvent) -> bool {
        let over = tui.over_messages(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if over => {
                self.follow_messages = false;
                self.message_scroll = if mouse.kind == MouseEventKind::ScrollUp {
                    tui.message_scroll().saturating_sub(1)
                } else {
                    tui.message_scroll()
                        .saturating_add(1)
                        .min(tui.message_max_scroll())
                };
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.overlay_gesture = over;
                if over {
                    self.view.clear_selection();
                }
                over
            }
            MouseEventKind::Drag(MouseButton::Left) if over || self.overlay_gesture => {
                self.overlay_gesture = true;
                self.view.clear_selection();
                true
            }
            MouseEventKind::Up(MouseButton::Left) if over || self.overlay_gesture => {
                self.overlay_gesture = false;
                self.view.clear_selection();
                true
            }
            _ => over,
        }
    }

    /// Expose the single pending prompt to key routing, completion, and status rendering.
    fn pending_prompt(&self) -> Option<&PendingPrompt> {
        match &self.turn.as_ref()?.phase {
            Phase::Running { prompt, .. } => prompt.as_ref(),
            _ => None,
        }
    }

    /// Shift+Tab: publish the new permission before persisting it; applies to the next operation.
    fn cycle_mode(&mut self) {
        let permission = self.settings.permission.next();
        self.settings.permission = permission;
        self.runtime.set_permission(permission);
        self.patch_runtime
            .set_permission(permission == config::Permission::ReadWrite);
        self.read_runtime
            .set_permission(permission != config::Permission::None);
        self.grep_runtime
            .set_permission(permission != config::Permission::None);
        self.list_runtime
            .set_permission(permission != config::Permission::None);
        if let Err(error) = config::save_permission(&self.settings.dir, permission) {
            self.notice = Some(format!("couldn't save permission: {error:#}"));
        }
    }

    /// `/tool-reasoning`: switch tool reasoning and persist it. The transcript
    /// changes at once and the next request is built accordingly, since both are
    /// derived from the session.
    fn toggle_tool_reasoning(&mut self) {
        let enabled = !self.settings.tool_reasoning;
        self.settings.tool_reasoning = enabled;
        if let Err(error) = config::save_tool_reasoning(&self.settings.dir, enabled) {
            self.notice = Some(format!("couldn't save tool_reasoning: {error:#}"));
        }
    }

    /// Run a command (typed in full, or picked from the completion list). The input
    /// is cleared, except after a failed `/rename`, which keeps it for correction like
    /// an unknown command.
    fn run_command(&mut self, action: Action, argument: &str) -> anyhow::Result<Flow> {
        match action {
            Action::Exit => return Ok(Flow::Exit),
            Action::ToggleToolReasoning => self.toggle_tool_reasoning(),
            Action::ShowTools => {
                // Local command output belongs to the display, never the saved conversation or model context.
                for (name, description) in
                    client::tool_descriptions(&self.settings.repl_tool_description)
                {
                    self.notes.push(Note {
                        anchor: Anchor::After(self.session.messages.len()),
                        heading: Some(name),
                        text: description,
                        style: self.settings.theme.status,
                    });
                }
                self.view.scroll_to_bottom();
            }
            Action::Rename => {
                if let Err(error) = self.session.rename(argument) {
                    self.notice = Some(format!("couldn't rename: {error:#}"));
                    return Ok(Flow::Continue);
                }
            }
        }
        self.textarea.clear();
        self.input_changed();
        Ok(Flow::Continue)
    }

    /// Both providers share the list; modal operations retain ownership of the footer.
    fn completions(&self) -> Vec<Completion> {
        if self.pending_prompt().is_some() || self.loading.is_some() {
            return Vec::new();
        }
        completion_candidates(
            &self.textarea,
            &self.file_completions,
            self.completion_dismissed.as_deref(),
        )
    }

    /// The input text changed: highlight the first completion again and reopen a
    /// list closed with Esc.
    fn input_changed(&mut self) {
        self.completion = 0;
        self.completion_dismissed = None;
        self.refresh_file_completions();
    }

    /// Read filenames only when an attachment prefix activates or changes, never on redraw.
    fn refresh_file_completions(&mut self) {
        let context =
            input::file_prefix(&self.textarea).map(|(row, prefix)| (row, prefix.to_owned()));
        if context == self.file_completion_context {
            return;
        }
        self.completion = 0;
        self.completion_dismissed = None;
        self.file_completions.clear();
        if let Some((_, prefix)) = &context {
            match injection::completions(&self.repo, prefix) {
                Ok(files) => self.file_completions = files,
                Err(error) => self.notice = Some(format!("couldn't complete files: {error:#}")),
            }
        }
        self.file_completion_context = context;
    }

    /// Rows below the input: the completion list while it shows, otherwise the
    /// status line.
    fn footer(&self) -> Vec<Line<'static>> {
        if self.loading.is_some() {
            return vec![Line::styled(
                "Loading files… (esc to cancel)",
                self.settings.theme.status,
            )];
        }
        let completions = self.completions();
        // A failed attachment must remain visible even when its typed prefix still has matches.
        if completions.is_empty()
            || (self.notice.is_some() && input::file_prefix(&self.textarea).is_some())
        {
            return vec![self.status_line()];
        }
        let selected = self.completion.min(completions.len() - 1);
        // A window of rows that keeps the highlighted one in view.
        let first = (selected + 1).saturating_sub(COMPLETION_ROWS);
        let width = completions
            .iter()
            .map(|completion| completion.spelling.len())
            .max()
            .unwrap_or(0);
        completions
            .iter()
            .enumerate()
            .skip(first)
            .take(COMPLETION_ROWS)
            .map(|(index, completion)| {
                // Indented to line up with the input text after the `> ` prompt.
                let name = format!("  {:width$}   ", completion.spelling);
                let description = completion.command.map_or("", |command| command.description);
                if index == selected {
                    let reversed = self.settings.theme.status.reversed();
                    Line::from(vec![
                        Span::styled(name, reversed),
                        Span::styled(description, reversed),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(name, self.settings.theme.status),
                        Span::styled(description, self.settings.theme.status),
                    ])
                }
            })
            .collect()
    }

    /// Commands run in any state and keep an unknown command in the input for
    /// correction. A message is sent only when no turn is in progress; otherwise the
    /// draft is kept.
    fn submit(&mut self, text: String) -> anyhow::Result<Flow> {
        if text.starts_with('/') {
            let command = text.trim_end();
            return match commands::find(command) {
                Some((action, argument)) => self.run_command(action, argument),
                None => {
                    self.notice = Some(format!("unknown command: {command}"));
                    Ok(Flow::Continue)
                }
            };
        }
        if self.turn.is_some() {
            self.notice = Some("turn in progress (esc to stop)".to_string());
            return Ok(Flow::Continue);
        }

        if self.loading.is_some() {
            return Ok(Flow::Continue);
        }
        let paths = injection::paths(&text);
        if paths.is_empty() {
            self.commit_submission(text)?;
        } else {
            self.loading = Some(PendingInjection::start(text, paths, self.repo.clone()));
        }
        Ok(Flow::Continue)
    }

    /// A failed attachment batch changes neither history nor draft; no partial files are sent.
    fn finish_injection(
        &mut self,
        result: anyhow::Result<Vec<PreparedInjection>>,
    ) -> anyhow::Result<()> {
        let Some(pending) = self.loading.take() else {
            return Ok(());
        };
        match result.and_then(|injections| self.session.inject(injections)) {
            Ok(()) => {
                // Attachment commands are local. Do not start a turn or save draft text as a message.
                self.textarea.clear();
                self.textarea
                    .insert_str(injection::remaining_text(&pending.text));
                self.input_changed();
                self.view.scroll_to_bottom();
                Ok(())
            }
            Err(error) => {
                self.notice = Some(format!("couldn't inject files: {error:#}"));
                Ok(())
            }
        }
    }

    /// Only an explicit ordinary message starts a turn and queues FYI before its request.
    fn commit_submission(&mut self, text: String) -> anyhow::Result<()> {
        if let Err(error) = self.session.submit(text) {
            self.notice = Some(format!("couldn't submit: {error:#}"));
            return Ok(());
        }
        self.textarea.clear();
        self.input_changed();
        self.view.scroll_to_bottom();
        // A new turn returns the compact session view to its newest positions.
        self.follow_messages = true;
        // FYI/help are the only per-turn cleanup cases. Tracked file injections persist.
        let removed = self
            .session
            .remove_calls(|call| code_of(call).is_ok_and(|code| is_exempt(&code)))?;
        self.messages_removed(&removed);
        // Each turn opens with an approval-exempt `FYI()` call made as if by the
        // model. `advance` runs it, then sends the first request.
        let calls = fyi_calls()?;
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
        Ok(())
    }

    /// Keep display-only note positions attached to surviving conversation messages.
    fn messages_removed(&mut self, removed: &[usize]) {
        // Notes placed after a message keep their place among the messages left.
        for note in &mut self.notes {
            if let Anchor::After(n) = &mut note.anchor {
                *n -= removed.partition_point(|&index| index < *n);
            }
        }
    }

    /// Show the current permission and the exact host operation awaiting approval.
    fn status_line(&self) -> Line<'static> {
        let mut info = self.settings.endpoint.model.clone();
        if let Some(tokens) = self.context_tokens {
            info.push_str(&format!(" | {} tokens", thousands(tokens)));
        }
        info.push_str(&format!(" | {}", self.settings.permission.as_str()));
        if self.settings.tool_reasoning {
            info.push_str(" | tool reasoning");
        }
        let status = self.settings.theme.status;
        let mut spans = vec![Span::styled(info, status)];
        let activity = match self.turn.as_ref().map(|t| &t.phase) {
            Some(Phase::Streaming(_)) => Some("responding… (esc to stop)"),
            Some(Phase::Patching { .. }) => Some("applying patch… (esc to stop)"),
            Some(Phase::Reading { .. }) => Some("reading… (esc to stop)"),
            Some(Phase::Searching { .. }) => Some("searching… (esc to stop)"),
            Some(Phase::Listing { .. }) => Some("listing… (esc to stop)"),
            Some(Phase::Running { prompt: None, .. }) | Some(Phase::Finishing(_)) => {
                Some("running… (esc to stop)")
            }
            _ => None,
        };
        if let Some(activity) = activity {
            spans.push(Span::styled(format!(" · {activity}"), status));
        }
        if let Some(prompt) = self.pending_prompt() {
            spans.push(Span::styled(" · ", status));
            spans.push(Span::styled(
                format!("allow {}? (y/n, esc to stop)", prompt.operation),
                self.settings.theme.approval,
            ));
        }
        if self.view.scrolled_up() {
            spans.push(Span::styled(" · scrolled up (PgDn)", status));
        }
        if let Some(notice) = &self.notice {
            spans.push(Span::styled(" · ", status));
            spans.push(Span::styled(notice.clone(), self.settings.theme.warning));
        }
        Line::from(spans)
    }
}

/// Project durable messages into ordered positions; tool results share their call's row.
fn message_overlay_entries(messages: &[Message]) -> anyhow::Result<Vec<String>> {
    let called: HashSet<&str> = messages
        .iter()
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .map(|call| call.id.as_str())
        .collect();
    let mut entries = Vec::new();
    let mut add = |label: String| {
        entries.push(format!("{} {label}", entries.len()));
    };
    for message in messages {
        match message.role {
            Role::User => add(format!("user {}", compact_preview(&message.content))),
            Role::Assistant => {
                // The header remains even when the assistant only made tool calls.
                let content = compact_preview(&message.content);
                add(if content.is_empty() {
                    "assistant".into()
                } else {
                    format!("assistant {content}")
                });
                if let Some(reasoning) = message.reasoning() {
                    add(format!("  reasoning {}", compact_preview(&reasoning)));
                } else if message
                    .extra
                    .get("reasoning_details")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| !blocks.is_empty())
                {
                    // Encrypted reasoning has no text preview, but still has a position.
                    add("  reasoning [opaque]".into());
                }
                for call in message.tool_calls.iter().flatten() {
                    add(format!("  tool {}", tool_preview(call)?));
                }
            }
            Role::Tool => {
                // A result without its call is still a saved message; keep it visible.
                if message
                    .tool_call_id
                    .as_deref()
                    .is_none_or(|id| !called.contains(id))
                {
                    add(format!(
                        "tool result {}",
                        message.tool_call_id.as_deref().unwrap_or("[no call id]")
                    ));
                }
            }
        }
    }
    Ok(entries)
}

/// Keep the full single-line preview until the overlay's available width clips it.
fn compact_preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Use the same decoded call content as the transcript, with attachment paths identified.
fn tool_preview(call: &ToolCall) -> anyhow::Result<String> {
    let content = if tool_call_id::decode(&call.id)
        .is_some_and(|metadata| metadata.kind == tool_call_id::INJECTED_FILE)
    {
        format!("@{}", injection::filename(call)?)
    } else if call.function.name == "apply_patch" {
        patch_of(call).unwrap_or_else(|_| call.function.arguments.clone())
    } else if matches!(call.function.name.as_str(), "read_file" | "grep" | "list") {
        call.function.arguments.clone()
    } else {
        code_of(call).unwrap_or_else(|_| call.function.arguments.clone())
    };
    Ok(format!(
        "{} {}",
        call.function.name,
        compact_preview(&content)
    ))
}

/// The transcript as drawn, so it always matches what the model sees: the system
/// message, the messages (each tool result under its call, matched by id, rather
/// than in file order), and the turn in progress. Notes appear where anchored.
///
/// Each block gets a `Key` naming what it shows, so the view's scroll position and
/// selection stay on the same text when blocks are inserted before it. A call's
/// result reuses the key of its live output, which it replaces.
fn build_blocks<'a>(
    system: Option<&'a str>,
    messages: &'a [Message],
    notes: &'a [Note],
    turn: Option<&'a Turn>,
    tool_reasoning_enabled: bool,
    theme: &Theme,
) -> Vec<Block<'a>> {
    let blank = |key| Block::new(key, "", theme.assistant);
    let turn_start = tool_reasoning::turn_start(messages);
    let mut blocks = Vec::new();
    if let Some(system) = system {
        blocks.push(Block::new(Key::System(0), "system", theme.system));
        blocks.push(Block::new(Key::System(1), system, theme.system));
        blocks.push(blank(Key::System(2)));
    }

    let results: HashMap<&str, &str> = messages
        .iter()
        .filter_map(|m| Some((m.tool_call_id.as_deref()?, m.content.as_str())))
        .collect();
    let mut call_notes: HashMap<&str, Vec<&Note>> = HashMap::new();
    let mut position_notes = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        match &note.anchor {
            Anchor::Call(id) => call_notes.entry(id).or_default().push(note),
            Anchor::After(n) => position_notes.push((*n, index, note)),
        }
    }
    // Notes are added in order, and cleanup keeps their positions in order.
    let mut position_notes = position_notes.into_iter().peekable();
    let mut add_position_notes = |through: usize, blocks: &mut Vec<Block<'a>>| {
        while let Some((_, index, note)) = position_notes.next_if(|&(n, ..)| n <= through) {
            if let Some(heading) = &note.heading {
                blocks.push(Block::new(
                    Key::Note(index, 2),
                    heading.as_str(),
                    theme.tool_result,
                ));
            }
            blocks.push(Block::new(
                Key::Note(index, 0),
                note.text.as_str(),
                note.style,
            ));
            blocks.push(blank(Key::Note(index, 1)));
        }
    };
    let (phase, running) = match turn.map(|t| &t.phase) {
        Some(Phase::Running {
            call,
            output,
            prompt,
        }) => (None, Some((&call.call.id, output, prompt.as_ref()))),
        phase => (phase, None),
    };

    for (i, message) in messages.iter().enumerate() {
        add_position_notes(i, &mut blocks);
        let part = |n| Key::Message(i, n);
        match message.role {
            Role::User => {
                let text = format!("> {}", message.content);
                blocks.push(Block::new(part(0), text, theme.user));
                blocks.push(blank(part(1)));
            }
            Role::Assistant => {
                // Converted reasoning is drawn as the synthetic call it's sent as, ahead
                // of the message. Drawn inline rather than as extra messages, so message
                // indices (keys, note anchors) match the session.
                let converted = tool_reasoning_enabled
                    .then(|| tool_reasoning::converted(messages, i, turn_start))
                    .flatten();
                if let Some(text) = &converted {
                    let part = |n| Key::Call(tool_reasoning::call_id(i, text), n);
                    blocks.push(Block::new(part(0), "REPL", theme.tool_call));
                    blocks.push(Block::new(
                        part(1),
                        tool_reasoning::code(text),
                        theme.reasoning,
                    ));
                    let result = tool_reasoning::RESULT;
                    blocks.push(Block::new(part(2), result, theme.tool_result));
                    blocks.push(blank(part(3)));
                }
                // Only reasoning, content, and calls are drawn; other fields are sent
                // back but have no display form.
                let reasoning = message.reasoning().filter(|_| converted.is_none());
                let has_reasoning = reasoning.is_some();
                if let Some(reasoning) = reasoning {
                    blocks.push(Block::new(part(0), reasoning, theme.reasoning));
                    if !message.content.is_empty() {
                        blocks.push(blank(part(1)));
                    }
                }
                if !message.content.is_empty() {
                    blocks.push(Block::new(
                        part(2),
                        message.content.as_str(),
                        theme.assistant,
                    ));
                }
                if has_reasoning || !message.content.is_empty() {
                    blocks.push(blank(part(3)));
                }
                for call in message.tool_calls.iter().flatten() {
                    // Parts: 0 label, 1 code, 2 result or live output, 3 blank, 4+ notes.
                    let part = |n| Key::Call(call.id.clone(), n);
                    match if call.function.name == "apply_patch" {
                        patch_of(call)
                    } else if matches!(call.function.name.as_str(), "read_file" | "grep" | "list") {
                        Ok(call.function.arguments.clone())
                    } else {
                        code_of(call)
                    } {
                        Ok(code) => {
                            blocks.push(Block::new(
                                part(0),
                                call.function.name.as_str(),
                                theme.tool_call,
                            ));
                            blocks.push(Block::new(part(1), code, theme.tool_call));
                        }
                        Err(_) => {
                            let arguments = call.function.arguments.as_str();
                            blocks.push(Block::new(part(0), arguments, theme.tool_call))
                        }
                    }
                    let call_notes = call_notes.get(call.id.as_str()).into_iter().flatten();
                    for (n, note) in call_notes.enumerate() {
                        let key = part(4 + n as u8);
                        blocks.push(Block::new(key, note.text.as_str(), note.style));
                    }
                    if let Some((id, _, Some(prompt))) = running
                        && *id == call.id
                        && let Some(description) = &prompt.description
                    {
                        // This wraps with the transcript and disappears with the pending approval.
                        blocks.push(Block::new(
                            Key::ApprovalDescription(call.id.clone()),
                            description.text.as_str(),
                            theme.approval,
                        ));
                    }
                    if let Some(result) = results.get(call.id.as_str()) {
                        blocks.push(Block::new(part(2), *result, theme.tool_result));
                    } else if let Some((id, output, _)) = running
                        && *id == call.id
                        && !output.is_empty()
                    {
                        blocks.push(Block::new(part(2), output.as_str(), theme.tool_result));
                    }
                    blocks.push(blank(part(3)));
                }
            }
            Role::Tool => {}
        }
    }
    add_position_notes(usize::MAX, &mut blocks);

    // Keyed as the message it will be saved as, so saving it changes no keys.
    if let Some(Phase::Streaming(reply)) = phase {
        let part = |n| Key::Message(messages.len(), n);
        let reasoning = reasoning_text(&reply.fields);
        let has_reasoning = reasoning.is_some();
        if let Some(reasoning) = reasoning {
            blocks.push(Block::new(part(0), reasoning, theme.reasoning));
        }
        let content = reply.fields.get("content").and_then(Value::as_str);
        if let Some(content) = content.filter(|content| !content.is_empty()) {
            if has_reasoning {
                blocks.push(blank(part(1)));
            }
            blocks.push(Block::new(part(2), content, theme.assistant));
        }
        if has_reasoning || content.is_some_and(|content| !content.is_empty()) {
            blocks.push(blank(part(3)));
        }
        for call in reply
            .fields
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let Some(name) = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            else {
                continue;
            };
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Reuse the completed call's keys; previews never enter execution or persistence.
            let part = |n| Key::Call(id.to_owned(), n);
            blocks.push(Block::new(part(0), name, theme.tool_call));
            let code = if name == "REPL" {
                streamed_code(arguments)
            } else if name == "apply_patch" {
                streamed_argument(arguments, "patch")
            } else {
                arguments.to_owned()
            };
            blocks.push(Block::new(part(1), code, theme.tool_call));
            blocks.push(blank(part(3)));
        }
    }
    blocks
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

/// The turn-start FYI call uses an ordinary ID with no timestamps or special markers.
fn fyi_calls() -> anyhow::Result<Vec<ToolCall>> {
    ["FYI()"]
        .into_iter()
        .map(|code| {
            Ok(ToolCall {
                id: tool_call_id::generate(tool_call_id::NORMAL, &[])?,
                kind: "function".to_string(),
                function: FunctionCall {
                    name: "REPL".to_string(),
                    arguments: format!(r#"{{"code": "{code}"}}"#),
                    extra: Map::new(),
                },
                extra: Map::new(),
            })
        })
        .collect()
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

/// Display the code available so far without accepting incomplete JSON for execution.
fn streamed_code(arguments: &str) -> String {
    streamed_argument(arguments, "code")
}

/// Both tools preview literal payloads without accepting partial JSON for execution.
fn streamed_argument(arguments: &str, field: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments);
    if let Ok(value) = &parsed {
        return value[field].as_str().unwrap_or(arguments).to_owned();
    }
    // Walk only top-level fields so nested or quoted keys cannot masquerade
    // as a tool payload. Complete preceding values use serde's parser.
    let preview = || -> Option<String> {
        let mut rest = arguments.trim_start().strip_prefix('{')?.trim_start();
        loop {
            if rest.is_empty() {
                return Some(String::new());
            }
            let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            let key = match keys.next()? {
                Ok(key) => key,
                Err(error) if error.is_eof() => return Some(String::new()),
                Err(_) => return None,
            };
            rest = rest[keys.byte_offset()..].trim_start();
            if rest.is_empty() {
                return Some(String::new());
            }
            rest = rest.strip_prefix(':')?.trim_start();
            if rest.is_empty() {
                return Some(String::new());
            }
            if key == field {
                let (code, closed) = streamed_string(rest)?;
                // A complete string in malformed JSON must remain visibly malformed.
                if closed && !parsed.as_ref().unwrap_err().is_eof() {
                    return None;
                }
                return Some(code);
            }
            let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            match values.next()? {
                Ok(_) => {}
                Err(error) if error.is_eof() => return Some(String::new()),
                Err(_) => return None,
            }
            rest = rest[values.byte_offset()..].trim_start();
            if rest.is_empty() {
                return Some(String::new());
            }
            rest = rest.strip_prefix(',')?.trim_start();
        }
    };
    preview().unwrap_or_else(|| arguments.to_owned())
}

/// Decode complete characters only; partial escapes and surrogate pairs wait for more data.
fn streamed_string(source: &str) -> Option<(String, bool)> {
    let mut chars = source.strip_prefix('"')?.chars();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some((output, true)),
            '\\' => {
                let Some(escape) = chars.next() else {
                    break;
                };
                let decoded = match escape {
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'u' => {
                        let Some(high) = streamed_hex(&mut chars)? else {
                            break;
                        };
                        if (0xd800..=0xdbff).contains(&high) {
                            // A high surrogate is one character only with its complete low half.
                            match chars.next() {
                                None => break,
                                Some('\\') => {}
                                _ => return None,
                            }
                            match chars.next() {
                                None => break,
                                Some('u') => {}
                                _ => return None,
                            }
                            let Some(low) = streamed_hex(&mut chars)? else {
                                break;
                            };
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return None;
                            }
                            char::from_u32(0x10000 + ((high - 0xd800) << 10) + low - 0xdc00)?
                        } else {
                            char::from_u32(high)?
                        }
                    }
                    _ => return None,
                };
                output.push(decoded);
            }
            ch if ch < '\u{20}' => return None,
            ch => output.push(ch),
        }
    }
    Some((output, false))
}

/// Distinguish an unfinished Unicode escape from an invalid hexadecimal digit.
fn streamed_hex(chars: &mut std::str::Chars<'_>) -> Option<Option<u32>> {
    let mut value = 0;
    for _ in 0..4 {
        let Some(ch) = chars.next() else {
            return Some(None);
        };
        value = value * 16 + ch.to_digit(16)?;
    }
    Some(Some(value))
}

/// Reject malformed patch arguments before the editor can touch the filesystem.
fn patch_of(call: &ToolCall) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        patch: String,
    }
    serde_json::from_str::<Arguments>(&call.function.arguments)
        .map(|args| args.patch)
        .map_err(|error| format!("Invalid apply_patch arguments: {error}"))
}

/// The `code` argument of a REPL call, or the error to return to the model.
fn code_of(call: &ToolCall) -> Result<String, String> {
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

/// `32093` → `32,093`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Await the active phase without consuming state when terminal input wins the select.
async fn next_turn_event(turn: &mut Option<Turn>) -> TurnEvent {
    let Some(turn) = turn else {
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
        Phase::Idle | Phase::Ready(_) => std::future::pending().await,
    }
}

/// Select one completion provider without performing filesystem I/O while drawing or handling keys.
fn completion_candidates(
    textarea: &TextArea,
    files: &[String],
    dismissed: Option<&str>,
) -> Vec<Completion> {
    if dismissed.is_some_and(|text| text == textarea.lines().join("\n")) {
        return Vec::new();
    }
    if input::file_prefix(textarea).is_some() {
        return files
            .iter()
            .map(|name| Completion {
                spelling: format!("@{name}"),
                command: None,
            })
            .collect();
    }
    let [text] = textarea.lines() else {
        return Vec::new();
    };
    if !text.starts_with('/') || text.contains(char::is_whitespace) {
        return Vec::new();
    }
    commands::completions(text)
        .into_iter()
        .map(|entry| Completion {
            spelling: entry.spelling.to_owned(),
            command: Some(entry.command),
        })
        .collect()
}

/// Poll only the current batch; canceled reads cannot later update local context.
async fn next_injection(
    pending: &mut Option<PendingInjection>,
) -> anyhow::Result<Vec<PreparedInjection>> {
    let Some(pending) = pending else {
        return std::future::pending().await;
    };
    (&mut pending.task)
        .await
        .context("joining file injection reads")?
}

/// Await only the current approval's request; cancellation never queues a stale result.
async fn next_description(prompt: &mut Option<PendingPrompt>) -> anyhow::Result<String> {
    let Some(task) = prompt
        .as_mut()
        .and_then(|prompt| prompt.description.as_mut())
        .and_then(|description| description.task.as_mut())
    else {
        return std::future::pending().await;
    };
    task.await.context("joining approval description request")?
}

/// Read terminal events on a dedicated thread. It polls with a timeout instead of
/// blocking in `read` so it notices when the app has stopped listening and exits.
fn spawn_input_reader() -> UnboundedReceiver<io::Result<Event>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(20)) {
                Ok(true) => {
                    if tx.send(event::read()).is_err() {
                        return;
                    }
                }
                Ok(false) => {
                    if tx.is_closed() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assistant parts keep their own positions while each call absorbs its result.
    #[test]
    fn message_overlay_groups_saved_messages() {
        let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user", "content":"hello\nthere"},
            {"role":"assistant", "content":"answer", "reasoning":"think\nmore",
             "tool_calls":[
                {"id":"first", "type":"function", "function":{"name":"list", "arguments":"{\"path\":\"src\"}"}},
                {"id":"second", "type":"function", "function":{"name":"REPL", "arguments":"{\"code\":\"print(1)\"}"}}
             ]},
            {"role":"tool", "tool_call_id":"first", "content":"hidden result"},
            {"role":"tool", "tool_call_id":"second", "content":"1"},
            {"role":"assistant", "content":"", "tool_calls":[
                {"id":"third", "type":"function", "function":{"name":"REPL", "arguments":"{\"code\":\"FYI()\"}"}}
            ]},
            {"role":"tool", "tool_call_id":"third", "content":"hidden again"}
        ])).unwrap();
        assert_eq!(
            message_overlay_entries(&messages).unwrap(),
            [
                "0 user hello there",
                "1 assistant answer",
                "2   reasoning think more",
                "3   tool list {\"path\":\"src\"}",
                "4   tool REPL print(1)",
                "5 assistant",
                "6   tool REPL FYI()",
            ]
        );
    }

    /// Provider IDs cannot introduce local markers; normalization preserves other call fields.
    #[test]
    fn provider_ids_and_results_use_normal_format() {
        let provider_ids = [
            "provider-call-id".to_owned(),
            tool_call_id::generate(tool_call_id::INJECTED_FILE, &[]).unwrap(),
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
            let request = tool_reasoning::request_messages(&messages, false);
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

    /// Turn start queues only FYI, using the normal ID layout.
    #[test]
    fn turn_start_calls_use_uniform_ids() {
        let calls = fyi_calls().unwrap();
        assert_eq!(calls.len(), 1);
        for (call, code) in calls.iter().zip(["FYI()"]) {
            assert_eq!(code_of(call).unwrap(), code);
            let metadata = tool_call_id::decode(&call.id).unwrap();
            assert_eq!(metadata.kind, tool_call_id::NORMAL);
            assert!(metadata.data.is_empty());
        }
    }

    /// The shared list preserves slash actions while file matches remain path edits only.
    #[test]
    fn completion_providers_and_dismissal() {
        let mut textarea = TextArea::default();
        textarea.insert_str("/q");
        let commands = completion_candidates(&textarea, &[], None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].spelling, "/quit");
        assert!(matches!(commands[0].command.unwrap().action, Action::Exit));
        assert!(completion_candidates(&textarea, &[], Some("/q")).is_empty());
        textarea.clear();
        textarea.insert_str("/rename");
        assert!(
            completion_candidates(&textarea, &[], None)[0]
                .command
                .unwrap()
                .takes_argument
        );
        textarea.insert_str(" session");
        assert!(completion_candidates(&textarea, &[], None).is_empty());

        textarea.clear();
        textarea.insert_str("Review\n@fi");
        let names = vec!["file".into(), "file with spaces".into()];
        let files = completion_candidates(&textarea, &names, None);
        assert_eq!(
            files
                .iter()
                .map(|entry| entry.spelling.as_str())
                .collect::<Vec<_>>(),
            ["@file", "@file with spaces"]
        );
        assert!(files.iter().all(|entry| entry.command.is_none()));
        assert!(completion_candidates(&textarea, &names, Some("Review\n@fi")).is_empty());
        assert!(completion_candidates(&textarea, &[], None).is_empty());
        textarea.move_cursor(ratatui_textarea::CursorMove::Head);
        assert!(completion_candidates(&textarea, &names, None).is_empty());
    }

    /// A failed asynchronous batch retains the exact held draft for correction.
    #[tokio::test]
    async fn failed_injection_keeps_held_draft() {
        let text = "Review these\n@\n trailing space ".to_string();
        let mut pending = Some(PendingInjection::start(
            text.clone(),
            injection::paths(&text),
            std::env::temp_dir(),
        ));
        let error = next_injection(&mut pending).await.err().unwrap();
        assert!(format!("{error:#}").contains("attachment path is empty"));
        assert_eq!(pending.as_ref().unwrap().text, text);
    }

    /// Abandoning a read cannot produce a later attachment event, even if its task was ready.
    #[tokio::test]
    async fn canceled_injection_cannot_submit_late() {
        let canceled = Arc::new(AtomicBool::new(false));
        let mut pending = Some(PendingInjection {
            text: "@file".into(),
            task: tokio::spawn(async { Ok(Vec::new()) }),
            canceled: canceled.clone(),
        });
        tokio::task::yield_now().await;
        pending.take();
        assert!(canceled.load(Ordering::Relaxed));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), next_injection(&mut pending))
                .await
                .is_err()
        );
    }

    /// Tool output keeps distinct styles and one blank separator, even with trailing REPL newlines.
    #[test]
    fn tools_output_before_first_message() {
        assert!(matches!(
            commands::find("/tools"),
            Some((Action::ShowTools, ""))
        ));
        assert!(commands::find("/tools extra").is_none());
        let completions = commands::completions("/tools");
        assert_eq!(completions.len(), 1);
        assert!(matches!(completions[0].command.action, Action::ShowTools));
        let theme = test_theme();
        let descriptions =
            client::tool_descriptions("Configured REPL description\n\nSecond paragraph\n");
        let notes: Vec<_> = descriptions
            .iter()
            .map(|(name, description)| Note {
                anchor: Anchor::After(0),
                heading: Some(name.clone()),
                text: description.clone(),
                style: theme.status,
            })
            .collect();
        let blocks = build_blocks(None, &[], &notes, None, false, &theme);
        assert_eq!(blocks.len(), descriptions.len() * 3);
        assert_ne!(theme.status.fg, theme.tool_result.fg);
        for (entry, (name, description)) in blocks.chunks_exact(3).zip(&descriptions) {
            assert_eq!(entry[0].text.as_ref(), name);
            assert_eq!(entry[0].style, theme.tool_result);
            assert_eq!(entry[1].text.as_ref(), description);
            assert_eq!(entry[1].style, theme.status);
            assert!(!entry[1].text.ends_with('\n'));
            assert!(entry[2].text.is_empty());
        }
        assert_eq!(
            blocks[1].text,
            "Configured REPL description\n\nSecond paragraph"
        );
    }

    /// Read the explicit example palette instead of maintaining hidden test defaults.
    fn test_theme() -> Theme {
        let config: toml::Value = toml::from_str(include_str!("../config.toml.example")).unwrap();
        config["theme"].clone().try_into().unwrap()
    }

    /// Read calls keep their path and range visible when saved and replayed.
    #[test]
    fn read_arguments_and_replay() {
        let arguments = r#"{"path":"src/main.rs","start_line":3,"end_line":5}"#;
        let args: read::Arguments = serde_json::from_str(arguments).unwrap();
        assert_eq!(args.start_line.unwrap().get(), 3);
        assert_eq!(args.end_line.unwrap().get(), 5);
        let all: read::Arguments = serde_json::from_str(r#"{"path":"src/main.rs"}"#).unwrap();
        assert!(all.start_line.is_none() && all.end_line.is_none());
        assert!(serde_json::from_str::<read::Arguments>(r#"{"path":"f","start_line":0}"#).is_err());
        let call: ToolCall = serde_json::from_value(serde_json::json!({
            "id": "read-test", "type": "function",
            "function": {"name": "read_file", "arguments": arguments}
        }))
        .unwrap();
        let messages = vec![
            Message {
                role: Role::Assistant,
                tool_calls: Some(vec![call]),
                ..Default::default()
            },
            Message::tool("read-test".into(), "3\tline three\n".into()),
        ];
        let blocks = build_blocks(None, &messages, &[], None, false, &test_theme());
        assert_eq!(blocks[0].text.as_ref(), "read_file");
        assert_eq!(blocks[1].text.as_ref(), arguments);
        assert_eq!(blocks[2].text.as_ref(), "3\tline three");
    }

    /// Listing calls replay their argument object and complete metadata rows without reformatting.
    #[test]
    fn list_arguments_replay() {
        let result = "\"src space/main.rs\"  128  2026-10-03T18:30:00.123456789Z";
        for parameters in [
            serde_json::json!({}),
            serde_json::json!({"path":"src space","recursive":true,"include":"*.rs"}),
        ] {
            let arguments = parameters.to_string();
            let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
                {"role":"assistant","content":"","tool_calls":[{"id":"list-test","type":"function","function":{"name":"list","arguments":arguments}}]},
                {"role":"tool","tool_call_id":"list-test","content":format!("{result}\n")}
            ])).unwrap();
            let blocks = build_blocks(None, &messages, &[], None, false, &test_theme());
            let visible: Vec<_> = blocks
                .iter()
                .map(|block| block.text.as_ref())
                .filter(|text| !text.is_empty())
                .collect();
            assert_eq!(visible, ["list", arguments.as_str(), result]);
        }
    }

    /// Structured and historical grep calls display their stored arguments without conversion.
    #[test]
    fn grep_arguments_replay() {
        for parameters in [
            serde_json::json!({"pattern":"this|that|\\w+","path":"src café","context":2,"include":"*.rs"}),
            serde_json::json!({"command":"grep -rn 'this|that' 'src café'"}),
        ] {
            let arguments = parameters.to_string();
            let call: ToolCall = serde_json::from_value(serde_json::json!({
                "id": "grep-test", "type": "function",
                "function": {"name": "grep", "arguments": arguments}
            }))
            .unwrap();
            let messages = vec![
                Message {
                    role: Role::Assistant,
                    tool_calls: Some(vec![call]),
                    ..Default::default()
                },
                Message::tool("grep-test".into(), "src café/f:2:this\n".into()),
            ];
            let saved = serde_json::to_string(&messages).unwrap();
            let restored: Vec<Message> = serde_json::from_str(&saved).unwrap();
            let blocks = build_blocks(None, &restored, &[], None, false, &test_theme());
            assert_eq!(blocks[0].text.as_ref(), "grep");
            assert_eq!(blocks[1].text.as_ref(), arguments);
            assert_eq!(blocks[2].text.as_ref(), "src café/f:2:this");
        }
    }

    /// Patch previews and saved calls expose the same literal text, including backslashes.
    #[test]
    fn patch_arguments_preview_and_replay() {
        let patch = "*** Begin Patch\n*** Add File: f\n+\\n \"café\"\n*** End Patch";
        let arguments = serde_json::json!({"patch": patch}).to_string();
        let mut previous = String::new();
        for end in 0..=arguments.len() {
            if !arguments.is_char_boundary(end) {
                continue;
            }
            let preview = streamed_argument(&arguments[..end], "patch");
            assert!(patch.starts_with(&preview), "{preview:?}");
            assert!(preview.starts_with(&previous));
            previous = preview;
        }
        assert_eq!(previous, patch);
        let call: ToolCall = serde_json::from_value(serde_json::json!({
            "id": "patch-test", "type": "function",
            "function": {"name": "apply_patch", "arguments": arguments}
        }))
        .unwrap();
        assert_eq!(patch_of(&call).unwrap(), patch);
        let messages = vec![Message {
            role: Role::Assistant,
            tool_calls: Some(vec![call.clone()]),
            ..Default::default()
        }];
        let blocks = build_blocks(None, &messages, &[], None, false, &test_theme());
        assert_eq!(blocks[0].text.as_ref(), "apply_patch");
        assert_eq!(blocks[1].text.as_ref(), patch);
        for invalid in [r#"{"patch":1}"#, r#"{"patch":"unfinished"#] {
            let mut malformed = call.clone();
            malformed.function.arguments = invalid.into();
            assert!(patch_of(&malformed).is_err());
        }
    }

    /// Every fragment boundary must display only a decoded prefix, including split escapes.
    #[test]
    fn tool_code_preview_handles_fragment_boundaries() {
        for arguments in [
            r#"{"code":"print(\"hi\")\npath = 'a\\b'\t# café"}"#,
            r#"{"code":"a\u00e9\ud83d\ude42b"}"#,
            r#"{"other":{"code":"wrong"},"co\u0064e":"right\ncode"}"#,
        ] {
            let expected: Value = serde_json::from_str(arguments).unwrap();
            let expected = expected["code"].as_str().unwrap();
            let mut previous = String::new();
            for end in 0..=arguments.len() {
                if !arguments.is_char_boundary(end) {
                    continue;
                }
                let preview = streamed_code(&arguments[..end]);
                assert!(
                    expected.starts_with(&preview),
                    "unexpected preview {preview:?} at {end}: {arguments}"
                );
                assert!(
                    preview.starts_with(&previous),
                    "preview regressed at {end}: {arguments}"
                );
                previous = preview;
            }
            assert_eq!(previous, expected);
        }
    }

    /// Malformed input stays visible, and a readable preview never makes a call executable.
    #[test]
    fn tool_code_preview_preserves_invalid_arguments() {
        for arguments in [
            r#"{"code":12}"#,
            r#"{"code":"bad\q"#,
            r#"{"code":"bad\udc00"#,
            r#"{"code":"ok",!"#,
            r#"{"other":"only"}"#,
        ] {
            assert_eq!(streamed_code(arguments), arguments);
        }
        let call = ToolCall {
            id: "partial".into(),
            kind: "function".into(),
            extra: Map::new(),
            function: FunctionCall {
                name: "REPL".into(),
                arguments: r#"{"code":"print(1)"#.into(),
                extra: Map::new(),
            },
        };
        assert_eq!(streamed_code(&call.function.arguments), "print(1)");
        assert!(code_of(&call).is_err());
    }

    /// Real delta accumulation keeps multiple previews distinct and matches completed-call keys.
    #[tokio::test]
    async fn tool_call_previews_match_completed_blocks() {
        let mut fields = Map::new();
        for delta in [
            serde_json::json!({"tool_calls":[{"index":0,"function":{"name":"REPL","arguments":"{\"code\":\"print("}}]}),
            serde_json::json!({"tool_calls":[{"index":1,"id":"second","type":"function","function":{"name":"REPL","arguments":"{\"code\":\"x = 2"}}]}),
            serde_json::json!({"tool_calls":[{"index":0,"id":"first","type":"function","function":{"arguments":"1)\"}"}},{"index":1,"function":{"arguments":"\"}"}}]}),
        ] {
            client::accumulate(&mut fields, delta.as_object().unwrap().clone());
        }
        let (_, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending());
        let turn = Turn {
            phase: Phase::Streaming(Reply {
                task,
                events,
                fields,
            }),
            queue: VecDeque::new(),
            repl: None,
        };
        let streamed = build_blocks(None, &[], &[], Some(&turn), false, &test_theme());
        assert_eq!(
            streamed
                .iter()
                .map(|block| block.text.as_ref())
                .collect::<Vec<_>>(),
            ["REPL", "print(1)", "", "REPL", "x = 2", ""]
        );
        assert!(streamed[1].key == Key::Call("first".into(), 1));
        assert!(streamed[4].key == Key::Call("second".into(), 1));
        let Phase::Streaming(reply) = &turn.phase else {
            unreachable!()
        };
        let calls = reply.fields["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| {
                let mut value = value.clone();
                value.as_object_mut().unwrap().remove("index");
                serde_json::from_value(value).unwrap()
            })
            .collect();
        let messages = vec![Message {
            role: Role::Assistant,
            tool_calls: Some(calls),
            ..Message::default()
        }];
        let completed = build_blocks(None, &messages, &[], None, false, &test_theme());
        assert!(
            streamed
                .iter()
                .map(|block| (&block.key, &block.text))
                .eq(completed.iter().map(|block| (&block.key, &block.text)))
        );
        reply.task.abort();
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

    /// Native and converted reasoning retain their own undimmed color beside answers and tools.
    #[tokio::test]
    async fn transcript_uses_theme_roles() {
        let theme = test_theme();
        let messages: Vec<Message> = serde_json::from_value(serde_json::json!([
            {"role":"user","content":"question"},
            {"role":"assistant","content":"answer","reasoning":"thinking", "tool_calls":[
                {"id":"call","type":"function","function":{"name":"REPL","arguments":"{\"code\":\"print(1)\"}"}}
            ]},
            {"role":"tool","tool_call_id":"call","content":"1"},
            {"role":"user","content":"next"}
        ])).unwrap();
        let blocks = build_blocks(Some("instructions"), &messages, &[], None, false, &theme);
        for (text, style) in [
            ("instructions", theme.system),
            ("> question", theme.user),
            ("thinking", theme.reasoning),
            ("answer", theme.assistant),
            ("print(1)", theme.tool_call),
            ("1", theme.tool_result),
        ] {
            let block = blocks.iter().find(|block| block.text == text).unwrap();
            assert_eq!(block.style, style);
            assert!(
                !block
                    .style
                    .add_modifier
                    .contains(ratatui::style::Modifier::DIM)
            );
        }
        let converted = build_blocks(None, &messages, &[], None, true, &theme);
        assert_eq!(
            converted
                .iter()
                .find(|block| block.text == "reasoning(\"thinking\")")
                .unwrap()
                .style,
            theme.reasoning
        );
        let (_, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(std::future::pending());
        let turn = Turn {
            phase: Phase::Streaming(Reply {
                task,
                events,
                fields: serde_json::json!({"reasoning":"live thought","content":"live answer"})
                    .as_object()
                    .unwrap()
                    .clone(),
            }),
            queue: VecDeque::new(),
            repl: None,
        };
        let live = build_blocks(None, &[], &[], Some(&turn), false, &theme);
        assert_eq!(
            live.iter()
                .find(|block| block.text == "live thought")
                .unwrap()
                .style,
            theme.reasoning
        );
        assert_eq!(
            live.iter()
                .find(|block| block.text == "live answer")
                .unwrap()
                .style,
            theme.assistant
        );
        if let Phase::Streaming(reply) = &turn.phase {
            reply.task.abort();
        }
    }

    /// Completion or failure updates only ephemeral text and cannot be polled a second time.
    #[tokio::test]
    async fn approval_description_completion_is_display_only() {
        for result in [
            Ok("Lists directory contents.".to_owned()),
            Err(anyhow::anyhow!("endpoint unavailable")),
        ] {
            let expected = if result.is_ok() {
                "Description: Lists directory contents."
            } else {
                "Description unavailable: endpoint unavailable"
            };
            let (answer, _) = oneshot::channel();
            let mut prompt = Some(PendingPrompt {
                operation: "operation".into(),
                answer,
                description: Some(ApprovalDescription {
                    task: Some(tokio::spawn(async move { result })),
                    text: "Generating description…".into(),
                }),
            });
            let result = next_description(&mut prompt).await;
            prompt
                .as_mut()
                .unwrap()
                .description
                .as_mut()
                .unwrap()
                .complete(result);
            assert!(matches!(
                futures::poll!(Box::pin(next_description(&mut prompt))),
                std::task::Poll::Pending
            ));
            let call: ToolCall = serde_json::from_value(serde_json::json!({"id":"call","type":"function","function":{"name":"REPL","arguments":"{\"code\":\"run(['ls'])\"}"}})).unwrap();
            let messages = vec![Message {
                role: Role::Assistant,
                tool_calls: Some(vec![call.clone()]),
                ..Message::default()
            }];
            let before = serde_json::to_value(&messages).unwrap();
            let mut turn = Turn {
                phase: Phase::Running {
                    call: PendingCall {
                        call,
                        code: "run(['ls'])".into(),
                    },
                    output: String::new(),
                    prompt,
                },
                queue: VecDeque::new(),
                repl: None,
            };
            let blocks = build_blocks(None, &messages, &[], Some(&turn), false, &test_theme());
            assert!(
                blocks
                    .iter()
                    .any(|block| block.key == Key::ApprovalDescription("call".into())
                        && block.text == expected)
            );
            assert!(blocks.iter().any(|block| block.text == "run(['ls'])"));
            assert_eq!(serde_json::to_value(&messages).unwrap(), before);
            if let Phase::Running { prompt, .. } = &mut turn.phase {
                prompt.take();
            }
            let blocks = build_blocks(None, &messages, &[], Some(&turn), false, &test_theme());
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block.key, Key::ApprovalDescription(_)))
            );
        }
    }
}
