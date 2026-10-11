//! Chat loop: input events, the agent stack, and the display.
//!
//! The App owns the shared resources and holds the agents as a stack (`agent.rs`
//! runs each agent's tool-calling turn): the root, then each active branch above its
//! parent. Only the agent on top is active; the App routes events to it and acts on
//! the outcomes it returns, creating a branch for an accepted `rlm` call and handing a
//! branch's answer to its parent when it ends.
//!
//! The screen is drawn from state every frame (`build_blocks`): the active agent's
//! messages, its turn in progress, and its display-only notes. Handlers only change
//! state; drawing is the loop's job.

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::Duration;

use anyhow::{Context, ensure};
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui_textarea::TextArea;
use serde_json::{Map, Value};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::agent::{
    Agent, Outcome, Phase, Resources, STOPPED, Turn, TurnEvent, append_line, code_of, patch_of,
};
use crate::client;
use crate::commands::{self, Action};
use crate::config::{self, Settings, Theme};
use crate::failures::FailureLog;
use crate::grep;
use crate::input::{self, InputAction};
use crate::list;
use crate::patch;
use crate::read;
use crate::repl::Runtime;
use crate::root::Library;
use crate::session::{Message, Role, Session, ToolCall, Turn as SessionTurn, reasoning_text};
use crate::token_count;
use crate::tool_call_id;
use crate::transcript::{Block, Key, View};
use crate::tui::{MessageEntry, MessageOverlay, Tui};

/// The stack's invariant: the root agent is never removed.
const ROOT_HELD: &str = "the agent stack always holds the root agent";

/// Most rows the completion list shows at once.
const COMPLETION_ROWS: usize = 8;

/// Bound one input batch so continuous wheel traffic cannot starve drawing or signals.
const MAX_WHEEL_BATCH: usize = 64;

/// Validate persistent failure history and start workers before taking over the terminal.
pub async fn run(settings: Settings, session: Session) -> anyhow::Result<()> {
    let failures = FailureLog::load(&settings.dir)?;
    let runtime = Runtime::new(&settings).await?;
    // Each direct-tool runtime holds the repository and store capabilities; the
    // permission level governs the repository only.
    let patch_runtime = patch::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        &settings.store_path,
        settings.permission == config::Permission::ReadWrite,
    )?;
    let read_runtime = read::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        &settings.store_path,
        settings.permission != config::Permission::None,
    )?;
    let grep_runtime = grep::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        &settings.store_path,
        settings.permission != config::Permission::None,
    )?;
    let list_runtime = list::Runtime::new(
        &std::env::current_dir().context("finding launch directory")?,
        &settings.store_path,
        settings.permission != config::Permission::None,
    )?;
    let mut tui = Tui::enter()?;
    let textarea = input::new_textarea(&settings.theme);
    let http = reqwest::Client::new();
    let token_counts = token_count::Counter::new(http.clone(), settings.endpoint.clone());
    let root = Frame::root(session, &settings);
    let mut app = App {
        resources: Resources {
            settings,
            http,
            runtime,
            patch_runtime,
            read_runtime,
            grep_runtime,
            list_runtime,
        },
        failures,
        frames: vec![root],
        library: None,
        token_counts,
        textarea,
        view: View::default(),
        notice: None,
        signaled: false,
        completion: 0,
        completion_dismissed: None,
        message_scroll: usize::MAX,
        follow_messages: false,
        messages_collapsed: false,
        overlay_gesture: false,
    };
    let result = app.run(&mut tui).await;
    app.resources.runtime.close().await;
    let exited = tui.exit();
    // After a signal the terminal may be gone, so restoring it can fail harmlessly.
    if !app.signaled {
        exited?;
    }
    result
}

struct App {
    /// Lent to each agent call.
    resources: Resources,
    failures: FailureLog,
    /// The agent stack: the root at the bottom, then each active branch above its
    /// parent; the top is the active agent, on screen and receiving input. Agents
    /// below the top wait in `Phase::Delegating`.
    frames: Vec<Frame>,
    /// This tree's library for agents at depth 2 and deeper: opened on first use
    /// (`library`) and dropped by `/rename` and `/new`, so the next use opens the
    /// renamed directory or the new tree's.
    library: Option<Library>,
    /// Counts the active agent's session; replaced whenever the top of the stack
    /// changes (`show_top`).
    token_counts: token_count::Counter,
    textarea: TextArea<'static>,
    view: View,
    /// UI-only message shown in the status line until the next key press.
    notice: Option<String>,
    /// Exiting because of a signal: the terminal may be gone, so a failure to
    /// restore it is ignored.
    signaled: bool,
    /// Highlighted row of the command completion list. Back to the first match
    /// whenever the input changes.
    completion: usize,
    /// Input text for which Esc closed the completion list; it stays closed until
    /// the input changes.
    completion_dismissed: Option<String>,
    /// First visible display position, held while idle or after manual scrolling.
    message_scroll: usize,
    /// Follow appended positions only until the user scrolls during this turn.
    follow_messages: bool,
    /// Visibility lasts across turns and agent switches; only `/new` reopens a collapsed
    /// overlay, as a launch shows it.
    messages_collapsed: bool,
    /// A gesture touching the overlay must not select or copy the transcript underneath it.
    overlay_gesture: bool,
}

/// One agent on the stack and the notes shown with its transcript. Notes anchor to its
/// messages, so they are drawn only while it is on screen and end with it.
struct Frame {
    agent: Agent,
    /// Display-only lines (failure counts, errors, `/tools`), never saved in the session.
    notes: Vec<Note>,
    /// A branch's display label: the `name` argument of the `rlm` call that started it,
    /// taken when the branch starts and held only here (the call's saved arguments are
    /// its one durable copy). `None` for the root, which `Frame::name` shows by its
    /// session name instead. Never an identity: the call ID names a branch's file, its
    /// `parent`, and its failure-log records.
    branch_name: Option<String>,
}

impl Frame {
    /// The bottom of the stack for root session `session`, at launch and after `/new`.
    /// The root is a pure delegator (`rlm`, `FYI`) with no mounts; only branches at
    /// depth 2 and deeper get the library.
    fn root(session: Session, settings: &Settings) -> Self {
        Frame {
            agent: Agent::new(session, settings, None),
            notes: Vec::new(),
            branch_name: None,
        }
    }

    /// The name the status line shows while this agent is on screen, and the context
    /// overlay's header shows for it while it is on the stack: a branch's
    /// `branch_name`; for the root, its session name (the tree directory name), read
    /// from the session each time so it follows `/rename`.
    fn name(&self) -> &str {
        match &self.branch_name {
            Some(name) => name,
            None => self.agent.session().name(),
        }
    }
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
    Tokens(Option<(String, crate::session::TurnTokenCache)>),
}

/// Wheel events batch only while they continue acting on the same independently scrolling view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelTarget {
    Transcript,
    Context,
}

/// Classified wheel input, independent of terminal geometry after classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WheelInput {
    target: WheelTarget,
    delta: isize,
}

enum Flow {
    Continue,
    Exit,
}

/// Classify one wheel event against the geometry from the frame currently on screen.
fn wheel_input(event: &Event, over_context: impl FnOnce(u16, u16) -> bool) -> Option<WheelInput> {
    let Event::Mouse(mouse) = event else {
        return None;
    };
    let delta = match mouse.kind {
        MouseEventKind::ScrollUp => -1,
        MouseEventKind::ScrollDown => 1,
        _ => return None,
    };
    Some(WheelInput {
        target: if over_context(mouse.column, mouse.row) {
            WheelTarget::Context
        } else {
            WheelTarget::Transcript
        },
        delta,
    })
}

/// Drain only immediately queued wheel input for the same target. The first different
/// event is returned intact for the next frame, preserving input order.
fn coalesce_wheel<F>(
    first: WheelInput,
    input: &mut UnboundedReceiver<io::Result<Event>>,
    classify: F,
) -> (WheelInput, Option<io::Result<Event>>)
where
    F: Fn(&Event) -> Option<WheelInput>,
{
    let mut combined = first;
    for _ in 1..MAX_WHEEL_BATCH {
        let next = match input.try_recv() {
            Ok(next) => next,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        };
        if let Ok(event) = &next
            && let Some(wheel) = classify(event)
            && wheel.target == combined.target
        {
            combined.delta += wheel.delta;
            continue;
        }
        return (combined, Some(next));
    }
    (combined, None)
}

/// Apply a signed row delta without leaving the current scrollable range.
fn bounded_scroll(position: usize, delta: isize, maximum: usize) -> usize {
    position.saturating_add_signed(delta).min(maximum)
}

impl App {
    async fn run(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let result = self.run_loop(tui).await;
        // Exiting mid-turn, including on an error or a signal, keeps the partial
        // reply and records results for its tool calls.
        let stopped = self.stop();
        result.and(stopped)
    }

    /// The frame on top of the stack: the active agent and its notes.
    fn top(&self) -> &Frame {
        self.frames.last().expect(ROOT_HELD)
    }

    /// The top frame, for changing the active agent or its notes.
    fn top_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect(ROOT_HELD)
    }

    /// The active agent: the top of the stack, on screen and receiving input.
    fn agent(&self) -> &Agent {
        &self.top().agent
    }

    /// The active agent, for operations the App performs on it directly.
    fn agent_mut(&mut self) -> &mut Agent {
        &mut self.top_mut().agent
    }

    /// The active agent together with the shared resources its calls receive.
    fn active(&mut self) -> (&mut Agent, &Resources) {
        (
            &mut self.frames.last_mut().expect(ROOT_HELD).agent,
            &self.resources,
        )
    }

    /// Whether a branch is running above the root.
    fn delegating(&self) -> bool {
        self.frames.len() > 1
    }

    /// This tree's library, `library/` of the root's tree, opened on first use; a clone
    /// goes to each agent created at depth 2 and deeper, for its `/library` mount and
    /// its direct tools' `root = "library"`.
    fn library(&mut self) -> anyhow::Result<Library> {
        if let Some(library) = &self.library {
            return Ok(library.clone());
        }
        let root = &self.frames.first().expect(ROOT_HELD).agent;
        let library = Library::open(&root.session().library_dir())?;
        self.library = Some(library.clone());
        Ok(library)
    }

    /// Esc, `/exit`, and app exit: stop every agent from the top of the stack down.
    /// The deepest agent records its stop as any stopped turn does; its partial
    /// response (or its complete final response, when stopped after that streamed;
    /// `partial_content`) followed by `[stopped by user]` on its own line becomes its
    /// parent's `rlm` result and passes up unchanged as each ancestor's. Each agent's
    /// calls that never ran get `[not run: turn stopped]` (`stop_turn`). A failed save
    /// doesn't stop the rest; the first error is returned.
    fn stop(&mut self) -> anyhow::Result<()> {
        let mut first_error: Option<anyhow::Error> = None;
        let mut answer: Option<String> = None;
        loop {
            let (agent, resources) = self.active();
            let mut results = Vec::new();
            if let Some(answer) = &answer {
                results.push(agent.finish_delegation(resources, answer.clone()));
            }
            let partial = agent.partial_content();
            results.push(agent.stop_turn(resources));
            for result in results {
                if let Err(error) = result {
                    first_error.get_or_insert(error);
                }
            }
            if !self.delegating() {
                break;
            }
            // Only the deepest agent's text is taken; ancestors pass it up unchanged.
            answer.get_or_insert_with(|| append_line(partial, STOPPED));
            self.frames.pop();
        }
        if answer.is_some()
            && let Err(error) = self.show_top()
        {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The display follows the agent now on top of the stack: its transcript from the
    /// bottom with no selection (the previous agent's blocks share keys with this
    /// one's), the overlay at its newest positions, and token counts of its own session.
    fn show_top(&mut self) -> anyhow::Result<()> {
        self.view.scroll_to_bottom();
        self.view.clear_selection();
        self.message_scroll = usize::MAX;
        self.follow_messages = true;
        self.token_counts = token_count::Counter::new(
            self.resources.http.clone(),
            self.resources.settings.endpoint.clone(),
        );
        // Counted now rather than at the next frame, so an FYI the agent reports
        // before then sums its own turns.
        self.count_tokens()
    }

    /// Reconcile the token counter with the active agent's session. Only completed
    /// turns are tokenized, so a branch, whose one turn is active while it is on
    /// screen, gets no token cache.
    fn count_tokens(&mut self) -> anyhow::Result<()> {
        let agent = &mut self.frames.last_mut().expect(ROOT_HELD).agent;
        let active_user = agent
            .turn()
            .and_then(|_| agent.session().turns.len().checked_sub(1));
        self.token_counts.refresh(agent.session_mut(), active_user)
    }

    /// Move the active agent forward until the agent on top waits on something, acting
    /// on what it reports: a branch it starts becomes the active agent, and a branch
    /// whose turn ended answers its parent, whose advance then continues.
    async fn advance(&mut self) -> anyhow::Result<()> {
        loop {
            if self.delegating()
                && let Some(answer) = self.agent().answer()
            {
                let answer = answer.to_owned();
                self.end_branch(answer)?;
                continue;
            }
            // Taken before advancing: advancing changes neither the agent's reported
            // usage nor the overlay counts, so a call it runs gets the same value.
            let prompt_tokens = self.prompt_token_total();
            let (agent, resources) = self.active();
            match agent.advance(resources, prompt_tokens).await {
                Ok(Outcome::None) => return Ok(()),
                Ok(outcome) => self.act(outcome)?,
                Err(error) => self.agent_failed(error)?,
            }
        }
    }

    /// Create the branch for the active agent's accepted `rlm` call `call_id`, push it,
    /// and open its turn with its synthetic turn-start calls queued (`turn_start_calls`).
    /// A branch whose session can't be created has no turn to end, so the parent gets
    /// `[branch failed: <error>]` as its result directly; a failure opening the turn
    /// fails the pushed branch like any other branch error (`fail_branch`). `name` is
    /// the call's display label for the branch while it is on screen.
    fn start_branch(&mut self, call_id: &str, task: String, name: String) -> anyhow::Result<()> {
        let agent = match self.create_branch(call_id, task) {
            Ok(agent) => agent,
            Err(error) => {
                let answer = format!("[branch failed: {error:#}]");
                let (agent, resources) = self.active();
                return match agent.finish_delegation(resources, answer) {
                    Ok(()) => Ok(()),
                    Err(error) => self.agent_failed(error),
                };
            }
        };
        self.frames.push(Frame {
            agent,
            notes: Vec::new(),
            branch_name: Some(name),
        });
        // A new session has nothing for the turn-start cleanup to remove. The display
        // switches only once the turn is open: a branch session without an active turn
        // would be tokenized, and branch files get no token cache.
        if let Err(error) = self.agent_mut().start_turn() {
            return self.agent_failed(error);
        }
        self.show_top()
    }

    /// A new idle branch one depth below the active agent: its session file in the
    /// tree, saved with `task` as the user message. At depth 2 and deeper it gets the
    /// tree's library.
    fn create_branch(&mut self, call_id: &str, task: String) -> anyhow::Result<Agent> {
        let mut session = self.agent().session().branch(call_id)?;
        let library = if session.depth() >= 2 {
            Some(self.library()?)
        } else {
            None
        };
        session.submit(task)?;
        Ok(Agent::new(session, &self.resources.settings, library))
    }

    /// Pop the branch on top and record `answer` as its parent's `rlm` result; the
    /// display returns to the parent. The answer is recorded first, so no later error
    /// can leave the call unanswered. An error recording it is the parent's own.
    fn end_branch(&mut self, answer: String) -> anyhow::Result<()> {
        self.frames.pop();
        let (agent, resources) = self.active();
        if let Err(error) = agent.finish_delegation(resources, answer) {
            return self.agent_failed(error);
        }
        self.show_top()
    }

    /// An error from the active agent's own step. The root's error ends the App, as it
    /// always has; a branch fails instead.
    fn agent_failed(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        if !self.delegating() {
            return Err(error);
        }
        self.fail_branch(&format!("{error:#}"))
    }

    /// The branch on top failed: its turn ends the way a stream error ends one (the
    /// partial reply saved, calls that never ran answered, the error kept out of its
    /// file), and its parent's result is the text streamed so far in the failed
    /// response (or the complete final response, when the failure came after it
    /// streamed; `partial_content`) followed by `[branch failed: <error>]` on its own
    /// line.
    fn fail_branch(&mut self, error: &str) -> anyhow::Result<()> {
        let (agent, resources) = self.active();
        // Taken before `stop_turn`, which discards it.
        let partial = agent.partial_content();
        let stopped = agent.stop_turn(resources);
        let answer = append_line(partial, &format!("[branch failed: {error}]"));
        let Err(save_error) = stopped else {
            return self.end_branch(answer);
        };
        // A save error while ending the turn is not discarded: it ends the App, as it
        // does when a stream error ends the root's turn. The parent still gets this
        // branch's failure as its result first (the branch failed; the user did not
        // stop it); the exit's stop path (`stop`) then records the remaining
        // ancestors' results. As in `stop`, the first error is the one returned: a
        // failed save recording the parent's result leaves the message in its session
        // for the next save.
        self.frames.pop();
        let (parent, resources) = self.active();
        let _ = parent.finish_delegation(resources, answer);
        Err(save_error)
    }

    /// Act on what the active agent reported. A failure is persisted under the
    /// agent's session name (the tree name for the root, the call ID for a branch)
    /// before its count is shown; a failed append must not claim a durable occurrence.
    fn act(&mut self, outcome: Outcome) -> anyhow::Result<()> {
        match outcome {
            Outcome::None => {}
            Outcome::StartBranch {
                call_id,
                task,
                name,
            } => return self.start_branch(&call_id, task, name),
            Outcome::Failure(failure) => {
                let agent = &self.frames.last().expect(ROOT_HELD).agent;
                let count = self.failures.record(
                    agent.session().name(),
                    failure.kind,
                    &failure.message,
                    &failure.code,
                )?;
                let first_line = failure.message.lines().next().unwrap_or("");
                self.note(
                    Anchor::Call(failure.call_id),
                    format!("{}: {first_line} (seen {count} times)", failure.kind),
                    self.resources.settings.theme.warning,
                );
            }
            Outcome::StreamError(error) => {
                if self.delegating() {
                    return self.fail_branch(&error);
                }
                let (agent, resources) = self.active();
                agent.stop_turn(resources)?;
                let end = Anchor::After(self.agent().session().message_count());
                self.note(
                    end,
                    format!("error: {error}"),
                    self.resources.settings.theme.error,
                );
            }
        }
        Ok(())
    }

    async fn run_loop(&mut self, tui: &mut Tui) -> anyhow::Result<()> {
        let mut input = spawn_input_reader();
        let mut pending_input = None;
        // Closing the terminal (SIGHUP) or a kill would otherwise skip cleanup,
        // orphaning the REPL and leaving tool calls without results. In raw mode
        // Ctrl+C is a key press, so SIGINT only arrives from outside.
        let mut hangup = signal(SignalKind::hangup())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;

        loop {
            self.count_tokens()?;
            // Field borrows below, not `top()`, so drawing can borrow other fields
            // mutably alongside the active agent.
            let (frame, below) = self.frames.split_last().expect(ROOT_HELD);
            let agent = &frame.agent;
            // The agents below the one on screen, root first: the overlay's header names
            // them before it.
            let ancestors: Vec<&str> = below.iter().map(Frame::name).collect();
            let messages =
                message_overlay_entries(&agent.session().turns, self.token_counts.turns())?;
            let transcript_messages: Vec<_> = agent.session().messages().collect();
            let footer = self.footer();
            // The on-screen agent's effective system message, by the same rule as its
            // requests (`Settings::system_prompt`).
            let blocks = build_blocks(
                self.resources
                    .settings
                    .system_prompt(agent.session().depth()),
                &transcript_messages,
                &frame.notes,
                agent.turn(),
                &self.resources.settings.theme,
            );
            tui.draw(
                &blocks,
                &mut self.view,
                &self.textarea,
                &footer,
                &self.resources.settings.theme,
                MessageOverlay {
                    entries: &messages,
                    scroll: if self.follow_messages {
                        usize::MAX
                    } else {
                        self.message_scroll
                    },
                    collapsed: self.messages_collapsed,
                    ancestors: &ancestors,
                    name: frame.name(),
                },
            )?;
            // Collapsing changes geometry, not the position to restore when reopened.
            if !self.messages_collapsed {
                self.message_scroll = tui.message_scroll();
            }
            // Capture the last followed position when a turn ends; idle changes keep it.
            if self.agent().turn().is_none() {
                self.follow_messages = false;
            }
            // Biased toward input so keys (Esc above all) are handled before more
            // output from a fast stream.
            let step = if let Some(event) = pending_input.take() {
                Step::Input(Some(event))
            } else {
                tokio::select! {
                    biased;
                    _ = hangup.recv() => Step::Signal,
                    _ = terminate.recv() => Step::Signal,
                    _ = interrupt.recv() => Step::Signal,
                    event = input.recv() => Step::Input(event),
                    event = self.frames.last_mut().expect(ROOT_HELD).agent.next_turn_event() => {
                        Step::Turn(event)
                    }
                    result = self.token_counts.next() => Step::Tokens(result),
                }
            };
            match step {
                Step::Tokens(Some((digest, cache))) => {
                    let agent = &mut self.frames.last_mut().expect(ROOT_HELD).agent;
                    self.token_counts
                        .finish(agent.session_mut(), &digest, cache)?;
                }
                Step::Tokens(None) => {}
                Step::Signal => {
                    self.signaled = true;
                    return Ok(());
                }
                Step::Input(None) => return Ok(()),
                Step::Input(Some(event)) => {
                    let event = event?;
                    let wheel = wheel_input(&event, |x, y| tui.over_messages(x, y));
                    if let Some(wheel) = wheel {
                        let (wheel, pending) = coalesce_wheel(wheel, &mut input, |event| {
                            wheel_input(event, |x, y| tui.over_messages(x, y))
                        });
                        pending_input = pending;
                        self.scroll_wheel(tui, wheel);
                    } else if let Flow::Exit = self.on_input(tui, event)? {
                        return Ok(());
                    }
                }
                Step::Turn(event) => {
                    let (agent, resources) = self.active();
                    match agent.on_event(resources, event) {
                        Ok(outcome) => self.act(outcome)?,
                        Err(error) => self.agent_failed(error)?,
                    }
                }
            }
            self.advance().await?;
        }
    }

    /// Attach a display-only note to the active agent's transcript without changing the
    /// messages sent to the model.
    fn note(&mut self, anchor: Anchor, text: String, style: Style) {
        self.top_mut().notes.push(Note {
            anchor,
            heading: None,
            text,
            style,
        });
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
                // While a call awaits approval, only y/n, Esc, and Shift+Tab act. The
                // prompt is the active agent's, whichever agent that is.
                if self.agent().pending_prompt().is_some() {
                    match key.code {
                        KeyCode::Char(answer @ ('y' | 'n')) => {
                            if let Err(error) = self.agent_mut().approve(answer == 'y') {
                                self.agent_failed(error)?;
                            }
                        }
                        KeyCode::Esc => self.stop()?,
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
                            self.textarea.clear();
                            self.textarea.insert_str(completions[selected].spelling);
                            self.input_changed();
                            return Ok(Flow::Continue);
                        }
                        KeyCode::Enter => {
                            let completion = &completions[selected];
                            if !completion.command.takes_argument {
                                return self.run_command(completion.command.action, "");
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
                let before = self.textarea.lines().to_vec();
                let action = input::handle_key(&mut self.textarea, key);
                if self.textarea.lines() != before.as_slice() {
                    self.input_changed();
                }
                match action {
                    InputAction::Submit(text) => return self.submit(text),
                    InputAction::Stop => self.stop()?,
                    InputAction::CycleMode => self.cycle_mode(),
                    InputAction::None => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => self.scroll_wheel(
                    tui,
                    WheelInput {
                        target: WheelTarget::Transcript,
                        delta: if mouse.kind == MouseEventKind::ScrollUp {
                            -1
                        } else {
                            1
                        },
                    },
                ),
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
            Event::Paste(text) if self.agent().pending_prompt().is_none() => {
                input::paste(&mut self.textarea, &text);
                self.input_changed();
            }
            // A resize needs nothing here: the next frame lays out at the new size.
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Apply one already-coalesced wheel delta to its independently bounded target.
    fn scroll_wheel(&mut self, tui: &Tui, wheel: WheelInput) {
        match wheel.target {
            WheelTarget::Transcript => self.view.scroll(wheel.delta),
            WheelTarget::Context if !self.messages_collapsed => {
                self.follow_messages = false;
                self.message_scroll =
                    bounded_scroll(tui.message_scroll(), wheel.delta, tui.message_max_scroll());
            }
            WheelTarget::Context => {}
        }
    }

    /// Consume overlay gestures before transcript selection, even when a drag leaves the box.
    fn message_mouse(&mut self, tui: &Tui, mouse: MouseEvent) -> bool {
        let over = tui.over_messages(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if over => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -1
                } else {
                    1
                };
                self.scroll_wheel(
                    tui,
                    WheelInput {
                        target: WheelTarget::Context,
                        delta,
                    },
                );
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.overlay_gesture = over;
                if over {
                    self.view.clear_selection();
                    if tui.message_toggle_at(mouse.column, mouse.row) {
                        self.messages_collapsed = !self.messages_collapsed;
                        // Reopening restores the last visible scroll position.
                        self.follow_messages = false;
                    } else if let Some(turn_index) = tui.message_turn_at(mouse.column, mouse.row)
                        && can_toggle_turn(
                            self.agent().turn().is_some(),
                            turn_index,
                            self.agent().session().turns.len(),
                        )
                        && let Err(error) = self.agent_mut().toggle_hidden(turn_index)
                    {
                        self.notice = Some(format!("couldn't change turn visibility: {error:#}"));
                    }
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

    /// Shift+Tab: publish the new permission before persisting it; applies to the next operation.
    fn cycle_mode(&mut self) {
        let resources = &mut self.resources;
        let permission = resources.settings.permission.next();
        resources.settings.permission = permission;
        resources.runtime.set_permission(permission);
        resources
            .patch_runtime
            .set_permission(permission == config::Permission::ReadWrite);
        resources
            .read_runtime
            .set_permission(permission != config::Permission::None);
        resources
            .grep_runtime
            .set_permission(permission != config::Permission::None);
        resources
            .list_runtime
            .set_permission(permission != config::Permission::None);
        if let Err(error) = config::save_permission(&resources.settings.dir, permission) {
            self.notice = Some(format!("couldn't save permission: {error:#}"));
        }
    }

    /// Run a command (typed in full, or picked from the completion list). The input
    /// is cleared, except after a failed `/rename` or a refused or failed `/new`, which
    /// keeps it like an unknown command.
    fn run_command(&mut self, action: Action, argument: &str) -> anyhow::Result<Flow> {
        match action {
            Action::Exit => return Ok(Flow::Exit),
            Action::ShowTools => {
                // Local command output belongs to the display, never the saved conversation
                // or model context. It lists the tools of the agent on screen.
                let style = self.resources.settings.theme.status;
                let frame = self.top_mut();
                let end = frame.agent.session().message_count();
                for (name, description) in client::tool_descriptions(frame.agent.tools()) {
                    frame.notes.push(Note {
                        anchor: Anchor::After(end),
                        heading: Some(name),
                        text: description,
                        style,
                    });
                }
                self.view.scroll_to_bottom();
            }
            Action::Rename => {
                // Branch agents hold the tree's library handles and write into the tree
                // directory, so it can't move under them.
                if self.delegating() {
                    self.notice = Some("couldn't rename: a subagent is running".to_string());
                    return Ok(Flow::Continue);
                }
                if let Err(error) = self.agent_mut().session_mut().rename(argument) {
                    self.notice = Some(format!("couldn't rename: {error:#}"));
                    return Ok(Flow::Continue);
                }
                // The tree directory moved; the next agent needing the library reopens it.
                self.library = None;
            }
            Action::New => {
                // Refused as Enter on a message is during a turn, keeping the input. A
                // running branch's ancestors wait in `Phase::Delegating`, so checking
                // every frame covers every turn on the stack.
                if self.frames.iter().any(|frame| frame.agent.turn().is_some()) {
                    self.notice = Some("turn in progress (esc to stop)".to_string());
                    return Ok(Flow::Continue);
                }
                // As at launch: named by its start time, created on disk at its first save.
                let session = match Session::new(&self.resources.settings.dir) {
                    Ok(session) => session,
                    Err(error) => {
                        self.notice = Some(format!("couldn't start a new session: {error:#}"));
                        return Ok(Flow::Continue);
                    }
                };
                // Every change to the idle root was saved as it happened, so dropping it
                // leaves its session on disk unchanged for `--resume`. Its notes end with
                // its frame, and its tree's library is not the new tree's.
                self.frames = vec![Frame::root(session, &self.resources.settings)];
                self.library = None;
                // The display starts over as at launch: `show_top` replaces the token
                // counter and returns the transcript and the overlay to their newest
                // positions; a collapsed overlay reopens.
                self.messages_collapsed = false;
                self.show_top()?;
            }
        }
        self.textarea.clear();
        self.input_changed();
        Ok(Flow::Continue)
    }

    /// The command completion list for the current input; an approval prompt retains
    /// ownership of the footer.
    fn completions(&self) -> Vec<commands::Completion> {
        if self.agent().pending_prompt().is_some() {
            return Vec::new();
        }
        completion_candidates(&self.textarea, self.completion_dismissed.as_deref())
    }

    /// The input text changed: highlight the first completion again and reopen a
    /// list closed with Esc.
    fn input_changed(&mut self) {
        self.completion = 0;
        self.completion_dismissed = None;
    }

    /// Rows below the input: the completion list while it shows, otherwise the
    /// status line.
    fn footer(&self) -> Vec<Line<'static>> {
        let completions = self.completions();
        if completions.is_empty() {
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
                let description = completion.command.description;
                let status = self.resources.settings.theme.status;
                if index == selected {
                    let reversed = status.reversed();
                    Line::from(vec![
                        Span::styled(name, reversed),
                        Span::styled(description, reversed),
                    ])
                } else {
                    Line::from(vec![
                        Span::styled(name, status),
                        Span::styled(description, status),
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
        if self.agent().turn().is_some() {
            self.notice = Some("turn in progress (esc to stop)".to_string());
            return Ok(Flow::Continue);
        }
        self.commit_submission(text)?;
        Ok(Flow::Continue)
    }

    /// Save a submitted message, which starts a turn and queues FYI before its request.
    fn commit_submission(&mut self, text: String) -> anyhow::Result<()> {
        if let Err(error) = self.agent_mut().session_mut().submit(text) {
            self.notice = Some(format!("couldn't submit: {error:#}"));
            return Ok(());
        }
        self.textarea.clear();
        self.input_changed();
        self.view.scroll_to_bottom();
        // A new turn returns the compact session view to its newest positions.
        self.message_scroll = usize::MAX;
        self.follow_messages = true;
        let removed = self.agent_mut().start_turn()?;
        self.messages_removed(&removed);
        Ok(())
    }

    /// Keep display-only note positions attached to surviving conversation messages.
    fn messages_removed(&mut self, removed: &[usize]) {
        // Notes placed after a message keep their place among the messages left.
        for note in &mut self.top_mut().notes {
            if let Anchor::After(n) = &mut note.anchor {
                *n -= removed.partition_point(|&index| index < *n);
            }
        }
    }

    /// Prefer the active agent's reported prompt usage, falling back to the context
    /// overlay's aggregate.
    fn prompt_token_total(&self) -> token_count::TokenTotal {
        preferred_token_total(self.agent().prompt_tokens(), self.token_counts.total())
    }

    /// Prefer the active agent's reported context usage, falling back to the context
    /// overlay's aggregate.
    fn context_token_total(&self) -> token_count::TokenTotal {
        preferred_token_total(self.agent().context_tokens(), self.token_counts.total())
    }

    /// Name the agent on screen (`Frame::name`: the session name for the root, the
    /// caller-given `name` for a branch), and show the current permission and the exact
    /// host operation awaiting approval.
    fn status_line(&self) -> Line<'static> {
        let settings = &self.resources.settings;
        let mut info = format!("{} | {}", self.top().name(), settings.endpoint.model);
        let tokens = self.context_token_total();
        info.push_str(&format!(" | {} tokens", status_token_count(tokens)));
        info.push_str(&format!(" | {}", settings.permission.as_str()));
        let status = settings.theme.status;
        let mut spans = vec![Span::styled(info, status)];
        let activity = match self.agent().turn().map(|t| &t.phase) {
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
        if let Some(prompt) = self.agent().pending_prompt() {
            spans.push(Span::styled(" · ", status));
            spans.push(Span::styled(
                format!("allow {}? (y/n, esc to stop)", prompt.operation),
                settings.theme.approval,
            ));
        }
        if self.view.scrolled_up() {
            spans.push(Span::styled(" · scrolled up (PgDn)", status));
        }
        if let Some(notice) = &self.notice {
            spans.push(Span::styled(" · ", status));
            spans.push(Span::styled(notice.clone(), settings.theme.warning));
        }
        Line::from(spans)
    }
}

/// Project durable messages into ordered positions; tool results share their call's row.
fn message_overlay_entries(
    session_turns: &[SessionTurn],
    counted_turns: &[token_count::CountedTurn],
) -> anyhow::Result<Vec<MessageEntry>> {
    let called: HashSet<&str> = session_turns
        .iter()
        .flat_map(|turn| &turn.messages)
        .filter_map(|message| message.tool_calls.as_ref())
        .flatten()
        .map(|call| call.id.as_str())
        .collect();
    let mut entries = Vec::new();
    for turn in counted_turns {
        let turn_index = turn.turn_index;
        let session_turn = &session_turns[turn_index];
        ensure!(
            turn.hidden == session_turn.hidden,
            "counted turn visibility is stale"
        );
        let estimate = if turn.cache.estimated { "~" } else { "" };
        entries.push(MessageEntry {
            turn_index,
            hidden: turn.hidden,
            prefix: String::new(),
            label: format!("Turn {turn_index}"),
            turn_label: true,
            tokens: format!("{estimate}{} tokens", thousands(turn.cache.total)),
            payload: String::new(),
        });
        let mut counts = turn.cache.positions.iter().copied();
        let mut add = |role: &str, payload: String, nested: bool| {
            let count = counts.next().unwrap_or(0);
            let indent = if nested { "  " } else { "" };
            entries.push(MessageEntry {
                turn_index,
                hidden: turn.hidden,
                prefix: indent.to_string(),
                label: role.to_string(),
                turn_label: false,
                tokens: format!("{estimate}{}t", thousands(count)),
                payload,
            });
        };
        for message in &session_turn.messages {
            match message.role {
                Role::User => add("user", compact_preview(&message.content), false),
                Role::Assistant => {
                    let visible_calls: Vec<_> = message
                        .tool_calls
                        .iter()
                        .flatten()
                        .filter(|call| {
                            tool_call_id::decode(&call.id)
                                .is_none_or(|metadata| metadata.kind != tool_call_id::SYNTHETIC)
                        })
                        .collect();
                    // A dedicated synthetic assistant message has no visible group content.
                    if message.content.is_empty()
                        && message.extra.is_empty()
                        && message
                            .tool_calls
                            .as_ref()
                            .is_some_and(|calls| !calls.is_empty() && visible_calls.is_empty())
                    {
                        continue;
                    }
                    // The header remains even when ordinary tool calls are its only content.
                    add("assistant", compact_preview(&message.content), false);
                    if let Some(reasoning) = message.reasoning() {
                        add("reasoning", compact_preview(&reasoning), true);
                    } else if message
                        .extra
                        .get("reasoning_details")
                        .and_then(Value::as_array)
                        .is_some_and(|blocks| !blocks.is_empty())
                    {
                        // Encrypted reasoning has no text preview, but still has a position.
                        add("reasoning", "[opaque]".into(), true);
                    }
                    for call in visible_calls {
                        add("tool", tool_preview(call), true);
                    }
                }
                Role::Tool => {
                    // A result without its call is still a saved message; keep it visible.
                    if message
                        .tool_call_id
                        .as_deref()
                        .is_none_or(|id| !called.contains(id))
                    {
                        add(
                            "tool result",
                            message
                                .tool_call_id
                                .as_deref()
                                .unwrap_or("[no call id]")
                                .into(),
                            false,
                        );
                    }
                }
            }
        }
        ensure!(
            counts.next().is_none(),
            "turn token cache has extra position counts"
        );
    }
    Ok(entries)
}

/// Keep the full single-line preview until the overlay's available width clips it.
fn compact_preview(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Tools whose argument objects display raw, as stored: the direct file tools, `rlm`,
/// and `FYI`. Their streaming previews show the same raw text.
fn shows_raw_arguments(name: &str) -> bool {
    matches!(name, "read_file" | "grep" | "list" | "rlm" | "FYI")
}

/// Use the same decoded call content as the transcript.
fn tool_preview(call: &ToolCall) -> String {
    let content = if call.function.name == "apply_patch" {
        patch_display(call).unwrap_or_else(|_| call.function.arguments.clone())
    } else if shows_raw_arguments(&call.function.name) {
        call.function.arguments.clone()
    } else {
        code_of(call).unwrap_or_else(|_| call.function.arguments.clone())
    };
    format!("{} {}", call.function.name, compact_preview(&content))
}

/// The display text of a complete apply_patch call, shared by the transcript and the
/// context overlay: its patch, preceded by the call's `root` argument (`with_root`) so
/// the directory a patch edits stays visible. Calls without `root`, including every
/// historical one, display only their patch. Errors as `patch_of` does, for raw display.
fn patch_display(call: &ToolCall) -> Result<String, String> {
    let patch = patch_of(call)?;
    // `patch_of` accepted the arguments, so they parse as a JSON object.
    let arguments: Value = serde_json::from_str(&call.function.arguments)
        .map_err(|error| format!("Invalid apply_patch arguments: {error}"))?;
    Ok(with_root(arguments.get("root"), patch))
}

/// Precede displayed patch text with a `root: <value>` line when the arguments include
/// `root`: a string as itself, any other value as its JSON text, since the call shows
/// what it named even when that is not a valid root.
fn with_root(root: Option<&Value>, patch: String) -> String {
    match root {
        None => patch,
        Some(Value::String(root)) => format!("root: {root}\n{patch}"),
        Some(root) => format!("root: {root}\n{patch}"),
    }
}

/// Draw the complete durable transcript, including turns excluded from model requests.
/// Each tool result stays under its call, matched by ID rather than file order, and
/// the turn in progress and display-only notes appear at their owned positions.
///
/// Each block gets a `Key` naming what it shows, so the view's scroll position and
/// selection stay on the same text when blocks are inserted before it. A call's
/// result reuses the key of its live output, which it replaces.
fn build_blocks<'a>(
    system: Option<&'a str>,
    messages: &[&'a Message],
    notes: &'a [Note],
    turn: Option<&'a Turn>,
    theme: &Theme,
) -> Vec<Block<'a>> {
    let blank = |key| Block::new(key, "", theme.assistant);
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
                // Only reasoning, content, and calls are drawn; other fields are sent
                // back but have no display form.
                let reasoning = message.reasoning();
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
                        patch_display(call)
                    } else if shows_raw_arguments(&call.function.name) {
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
                streamed_patch(arguments)
            } else {
                arguments.to_owned()
            };
            blocks.push(Block::new(part(1), code, theme.tool_call));
            blocks.push(blank(part(3)));
        }
    }
    blocks
}

/// Display the code available so far without accepting incomplete JSON for execution.
fn streamed_code(arguments: &str) -> String {
    streamed_argument(arguments, "code")
}

/// Preview apply_patch as its completed call displays (`patch_display`): the patch so
/// far, preceded by the `root` line once a complete `root` value has arrived. A `root`
/// streamed after the patch appears when the arguments complete.
fn streamed_patch(arguments: &str) -> String {
    match streamed_fields(arguments, "patch") {
        Some((patch, complete)) => with_root(complete.get("root"), patch),
        None => arguments.to_owned(),
    }
}

/// Preview a literal payload alone without accepting partial JSON for execution;
/// malformed arguments show raw.
fn streamed_argument(arguments: &str, field: &str) -> String {
    streamed_fields(arguments, field)
        .map(|(payload, _)| payload)
        .unwrap_or_else(|| arguments.to_owned())
}

/// The string payload `field` decoded so far, with the top-level members before it
/// whose values are already complete (every member, once the arguments parse). None
/// means the arguments are malformed or the payload is not a string; the caller shows
/// them raw.
fn streamed_fields(arguments: &str, field: &str) -> Option<(String, Map<String, Value>)> {
    let parse_error = match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(members)) => {
            let payload = members.get(field)?.as_str()?.to_owned();
            return Some((payload, members));
        }
        Ok(_) => return None,
        Err(error) => error,
    };
    // Walk only top-level fields so nested or quoted keys cannot masquerade
    // as a tool payload. Complete preceding values use serde's parser.
    let mut members = Map::new();
    let mut rest = arguments.trim_start().strip_prefix('{')?.trim_start();
    loop {
        if rest.is_empty() {
            return Some((String::new(), members));
        }
        let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let key = match keys.next()? {
            Ok(key) => key,
            Err(error) if error.is_eof() => return Some((String::new(), members)),
            Err(_) => return None,
        };
        rest = rest[keys.byte_offset()..].trim_start();
        if rest.is_empty() {
            return Some((String::new(), members));
        }
        rest = rest.strip_prefix(':')?.trim_start();
        if rest.is_empty() {
            return Some((String::new(), members));
        }
        if key == field {
            let (payload, closed) = streamed_string(rest)?;
            // A complete string in malformed JSON must remain visibly malformed.
            if closed && !parse_error.is_eof() {
                return None;
            }
            return Some((payload, members));
        }
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        match values.next()? {
            Ok(value) => {
                members.insert(key, value);
            }
            Err(error) if error.is_eof() => return Some((String::new(), members)),
            Err(_) => return None,
        }
        rest = rest[values.byte_offset()..].trim_start();
        if rest.is_empty() {
            return Some((String::new(), members));
        }
        rest = rest.strip_prefix(',')?.trim_start();
    }
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

/// Endpoint usage is authoritative; overlay-derived totals fill only its absence.
fn preferred_token_total(
    reported: Option<u64>,
    fallback: token_count::TokenTotal,
) -> token_count::TokenTotal {
    reported
        .map(|count| token_count::TokenTotal {
            count,
            estimated: false,
        })
        .unwrap_or(fallback)
}

/// The final displayed turn is immutable only while it is the active execution turn.
fn can_toggle_turn(active: bool, turn_index: usize, turn_count: usize) -> bool {
    !active || turn_index != turn_count.saturating_sub(1)
}

/// Format the status value with grouping and the overlay's estimate marker.
fn status_token_count(tokens: token_count::TokenTotal) -> String {
    let estimate = if tokens.estimated { "~" } else { "" };
    format!("{estimate}{}", thousands(tokens.count))
}

/// Command completions for one-line input starting with `/` and no whitespace, unless
/// Esc dismissed the list for this exact input.
fn completion_candidates(
    textarea: &TextArea,
    dismissed: Option<&str>,
) -> Vec<commands::Completion> {
    if dismissed.is_some_and(|text| text == textarea.lines().join("\n")) {
        return Vec::new();
    }
    let [text] = textarea.lines() else {
        return Vec::new();
    };
    if !text.starts_with('/') || text.contains(char::is_whitespace) {
        return Vec::new();
    }
    commands::completions(text)
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
    use std::collections::VecDeque;

    use serde_json::Map;
    use tokio::sync::oneshot;

    use super::*;
    use crate::agent::{
        ApprovalDescription, PendingCall, PendingPrompt, Reply, next_description, turn_start_calls,
    };
    use crate::host::Mounts;
    use crate::session::FunctionCall;

    /// Reported usage remains exact and takes precedence over an estimated overlay total.
    #[test]
    fn reported_token_usage_precedes_fallback() {
        let fallback = token_count::TokenTotal {
            count: 900,
            estimated: true,
        };
        assert_eq!(
            preferred_token_total(Some(123), fallback),
            token_count::TokenTotal {
                count: 123,
                estimated: false,
            }
        );
        assert_eq!(preferred_token_total(None, fallback), fallback);
    }

    /// The main status keeps digit grouping and exposes heuristic totals with `~`.
    #[test]
    fn status_token_count_marks_estimates() {
        assert_eq!(
            status_token_count(token_count::TokenTotal {
                count: 12_345,
                estimated: false,
            }),
            "12,345"
        );
        assert_eq!(
            status_token_count(token_count::TokenTotal {
                count: 12_345,
                estimated: true,
            }),
            "~12,345"
        );
    }

    /// Build one wheel event at an x-coordinate that selects the test target.
    fn wheel(kind: MouseEventKind, column: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    /// Test geometry treats the first ten columns as the context editor.
    fn classify_wheel(event: &Event) -> Option<WheelInput> {
        wheel_input(event, |column, _| column < 10)
    }

    /// Same-target wheel events combine exactly; a target change stays pending in order.
    #[test]
    fn wheel_batches_preserve_distance_direction_and_target_order() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        sender
            .send(Ok(wheel(MouseEventKind::ScrollDown, 20)))
            .unwrap();
        sender
            .send(Ok(wheel(MouseEventKind::ScrollUp, 20)))
            .unwrap();
        sender
            .send(Ok(wheel(MouseEventKind::ScrollDown, 5)))
            .unwrap();
        sender.send(Ok(Event::Resize(80, 24))).unwrap();

        let first = classify_wheel(&wheel(MouseEventKind::ScrollDown, 20)).unwrap();
        let (combined, pending) = coalesce_wheel(first, &mut receiver, classify_wheel);
        assert_eq!(
            combined,
            WheelInput {
                target: WheelTarget::Transcript,
                delta: 1,
            }
        );
        assert_eq!(
            classify_wheel(pending.as_ref().unwrap().as_ref().unwrap()),
            Some(WheelInput {
                target: WheelTarget::Context,
                delta: 1,
            })
        );
        assert!(matches!(
            receiver.try_recv().unwrap().unwrap(),
            Event::Resize(80, 24)
        ));
    }

    /// Non-wheel input ends a batch, bounds saturate, and the batch cap leaves work queued.
    #[test]
    fn wheel_batches_preserve_nonwheel_input_and_redraw_fairness() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        sender.send(Ok(Event::Resize(80, 24))).unwrap();
        let first = classify_wheel(&wheel(MouseEventKind::ScrollUp, 5)).unwrap();
        let (combined, pending) = coalesce_wheel(first, &mut receiver, classify_wheel);
        assert_eq!(combined.delta, -1);
        assert!(matches!(pending.unwrap().unwrap(), Event::Resize(80, 24)));
        assert_eq!(bounded_scroll(2, -5, 10), 0);
        assert_eq!(bounded_scroll(9, 5, 10), 10);

        for _ in 1..=MAX_WHEEL_BATCH + 5 {
            sender
                .send(Ok(wheel(MouseEventKind::ScrollDown, 20)))
                .unwrap();
        }
        let first = classify_wheel(&wheel(MouseEventKind::ScrollDown, 20)).unwrap();
        let (combined, pending) = coalesce_wheel(first, &mut receiver, classify_wheel);
        assert_eq!(combined.delta, MAX_WHEEL_BATCH as isize);
        assert!(pending.is_none());
        assert!(receiver.try_recv().is_ok());
    }

    /// Context clicks cannot hide the active final turn but retain every completed target.
    #[test]
    fn active_turn_is_not_toggleable() {
        assert!(!can_toggle_turn(true, 2, 3));
        assert!(can_toggle_turn(true, 1, 3));
        assert!(can_toggle_turn(false, 2, 3));
    }

    /// Derive deterministic estimates without making tokenizer requests in UI tests.
    fn counted_turns(turns: &[SessionTurn]) -> Vec<token_count::CountedTurn> {
        let endpoint = config::ResolvedEndpoint {
            url: "http://example.test/v1/chat/completions".into(),
            tokenize_url: "http://example.test/tokenize".into(),
            model: "test".into(),
            api_key: None,
            parameters: Map::new(),
        };
        token_count::plans(turns, &endpoint)
            .unwrap()
            .into_iter()
            .map(|plan| token_count::CountedTurn {
                turn_index: plan.turn_index,
                hidden: turns[plan.turn_index].hidden,
                cache: token_count::cached_or_estimated(&plan, &[]),
            })
            .collect()
    }

    /// Most UI fixtures contain one ordinary turn.
    fn one_turn(messages: Vec<Message>) -> Vec<SessionTurn> {
        vec![SessionTurn {
            messages,
            hidden: false,
        }]
    }

    /// Adapt owned message fixtures to the transcript's zero-copy reference view.
    fn message_refs(messages: &[Message]) -> Vec<&Message> {
        messages.iter().collect()
    }

    /// Assistant parts keep their own positions while each call absorbs its result.
    #[test]
    fn message_overlay_groups_saved_messages() {
        let mut messages: Vec<Message> = serde_json::from_value(serde_json::json!([
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
        let synthetic = turn_start_calls(1, Mounts::at(1, false)).unwrap().remove(0);
        let synthetic_id = synthetic.id.clone();
        messages.push(Message {
            role: Role::Assistant,
            tool_calls: Some(vec![synthetic]),
            ..Default::default()
        });
        messages.push(Message::tool(
            synthetic_id,
            "hidden synthetic result".into(),
        ));
        let session_turns = one_turn(messages);
        let turns = counted_turns(&session_turns);
        let rows = message_overlay_entries(&session_turns, &turns).unwrap();
        let visible: Vec<_> = rows
            .iter()
            .filter(|entry| !entry.turn_label)
            .map(|entry| {
                format!(
                    "{}{}{}{}",
                    entry.prefix,
                    entry.label,
                    if entry.payload.is_empty() { "" } else { " " },
                    entry.payload
                )
            })
            .collect();
        assert_eq!(
            visible,
            [
                "user hello there",
                "assistant answer",
                "  reasoning think more",
                "  tool list {\"path\":\"src\"}",
                "  tool REPL print(1)",
                "assistant",
                "  tool REPL FYI()",
            ]
        );
    }

    /// Slash completions carry their commands' actions and close on dismissal or an argument.
    #[test]
    fn completion_providers_and_dismissal() {
        let mut textarea = TextArea::default();
        textarea.insert_str("/q");
        let commands = completion_candidates(&textarea, None);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].spelling, "/quit");
        assert!(matches!(commands[0].command.action, Action::Exit));
        assert!(completion_candidates(&textarea, Some("/q")).is_empty());
        textarea.clear();
        textarea.insert_str("/rename");
        assert!(
            completion_candidates(&textarea, None)[0]
                .command
                .takes_argument
        );
        textarea.insert_str(" session");
        assert!(completion_candidates(&textarea, None).is_empty());
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
        let descriptions = client::tool_descriptions(&client::tool_definitions(
            1,
            1,
            "Configured REPL description\n\nSecond paragraph\n",
            Mounts::at(1, false),
        ));
        let notes: Vec<_> = descriptions
            .iter()
            .map(|(name, description)| Note {
                anchor: Anchor::After(0),
                heading: Some(name.clone()),
                text: description.clone(),
                style: theme.status,
            })
            .collect();
        let blocks = build_blocks(None, &[], &notes, None, &theme);
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
        let blocks = build_blocks(None, &message_refs(&messages), &[], None, &test_theme());
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
            let blocks = build_blocks(None, &message_refs(&messages), &[], None, &test_theme());
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
            let blocks = build_blocks(None, &message_refs(&restored), &[], None, &test_theme());
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
            let preview = streamed_patch(&arguments[..end]);
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
        let blocks = build_blocks(None, &message_refs(&messages), &[], None, &test_theme());
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
        let streamed = build_blocks(None, &[], &[], Some(&turn), &test_theme());
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
        let completed = build_blocks(None, &message_refs(&messages), &[], None, &test_theme());
        assert!(
            streamed
                .iter()
                .map(|block| (&block.key, &block.text))
                .eq(completed.iter().map(|block| (&block.key, &block.text)))
        );
        reply.task.abort();
    }

    /// Reasoning retains its own undimmed color beside answers and tools.
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
        let blocks = build_blocks(
            Some("instructions"),
            &message_refs(&messages),
            &[],
            None,
            &theme,
        );
        let hidden_turn = SessionTurn {
            messages: messages.clone(),
            hidden: true,
        };
        let hidden_blocks = build_blocks(
            Some("instructions"),
            &message_refs(&hidden_turn.messages),
            &[],
            None,
            &theme,
        );
        assert!(
            blocks
                .iter()
                .map(|block| (&block.text, block.style))
                .eq(hidden_blocks.iter().map(|block| (&block.text, block.style)))
        );
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
        let live = build_blocks(None, &[], &[], Some(&turn), &theme);
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
            let blocks = build_blocks(
                None,
                &message_refs(&messages),
                &[],
                Some(&turn),
                &test_theme(),
            );
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
            let blocks = build_blocks(
                None,
                &message_refs(&messages),
                &[],
                Some(&turn),
                &test_theme(),
            );
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block.key, Key::ApprovalDescription(_)))
            );
        }
    }
}
