//! Per-turn Monty sessions. A driver task owns the checkout and all host suspensions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use monty_pool::{
    Checkout, MountSpec, MountSpecMode, Pool, PoolConfig, PoolError, ReplConfig, ResumeValue,
    TurnEvent, on_print_sync,
};
use monty_types::{ExcType, MontyObject, NameLookupResult, NamedValues, ResourceLimits};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::config::{Permission, Settings};
use crate::host::{self, HostConfig, HostContext};
use crate::hostfs::{self, FsAction};
use crate::token_count::TokenTotal;

pub enum ReplEvent {
    /// Code output in execution order. The app applies its existing result truncation.
    Output(String),
    /// Answer only this suspension; dropping the sender cancels the pending operation.
    Prompt {
        operation: String,
        /// Only the resolved pending operation, never its enclosing REPL snippet.
        description_input: String,
        answer: oneshot::Sender<bool>,
    },
    /// Facts for the app's persistent failure log and display-only occurrence note.
    Failure {
        kind: &'static str,
        message: String,
        code: String,
    },
    /// Exactly one completion follows each accepted call.
    Done {
        value: Option<String>,
        error: Option<String>,
    },
}

/// App-lifetime pool and settings. The app creates this before entering the TUI.
pub struct Runtime {
    pool: Arc<Pool>,
    config: Arc<HostConfig>,
    repl_config: Arc<ReplConfig>,
    repo: PathBuf,
    /// Clone the opened directory capability across feeds; never reopen its path.
    mount: MountSpec,
    permission: watch::Sender<Permission>,
    memory_mb: usize,
}

impl Runtime {
    /// Start one idle worker and validate the repository and byte-limit conversion once.
    pub async fn new(settings: &Settings) -> anyhow::Result<Self> {
        let repo = std::env::current_dir()
            .context("finding repository directory")?
            .canonicalize()
            .context("resolving repository directory")?;
        let mount = MountSpec::new(
            repo.to_str()
                .context("repository directory is not valid UTF-8")?,
            &repo,
            MountSpecMode::ReadWrite,
        )
        .context("opening repository mount")?;
        let bytes = settings
            .max_memory_mb
            .checked_mul(1024 * 1024)
            .context("repl.max_memory_mb is too large to represent in bytes")?;
        let mut pool_config =
            PoolConfig::subprocess(std::env::current_exe().context("locating harness executable")?);
        pool_config.min_processes = 1;
        pool_config.max_processes = 1;
        let pool = Pool::new(pool_config)
            .await
            .context("starting Monty worker pool")?;
        let repl_config = ReplConfig {
            limits: Some(
                ResourceLimits::default()
                    .max_memory(bytes)
                    .max_suspensions(usize::MAX),
            ),
            // Monty's typing history loses partially executed snippets and narrows
            // earlier bindings. Use the interpreter's surviving state on every call;
            // host argument validation and per-operation permissions still apply.
            type_check: false,
            type_check_stubs: None,
            ..ReplConfig::default()
        };
        let (permission, _) = watch::channel(settings.permission);
        Ok(Self {
            pool: Arc::new(pool),
            config: Arc::new(HostConfig::new(settings)),
            repl_config: Arc::new(repl_config),
            repo,
            mount,
            permission,
            memory_mb: settings.max_memory_mb,
        })
    }

    /// Publish the effective level immediately; each operation reads the latest value.
    pub fn set_permission(&self, permission: Permission) {
        self.permission.send_replace(permission);
    }

    /// A fresh driver per turn; checkout and library load occur with its first call.
    pub fn start(&self) -> Repl {
        let (commands, incoming) = mpsc::unbounded_channel();
        let (events, output) = mpsc::unbounded_channel();
        let driver = Driver {
            pool: self.pool.clone(),
            config: self.repl_config.clone(),
            checkout: None,
            mount: self.mount.clone(),
            memory_mb: self.memory_mb,
            host: HostContext {
                repo: self.repo.clone(),
                config: self.config.clone(),
                permissions: self.permission.subscribe(),
                events,
                library: BTreeMap::new(),
                prompt_tokens: None,
                code: String::new(),
            },
        };
        Repl {
            commands,
            events: output,
            task: Some(tokio::spawn(driver.run(incoming))),
        }
    }

    /// Close idle workers after all turns have finished or been canceled.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

enum DriverCommand {
    Call {
        code: String,
        prompt_tokens: TokenTotal,
    },
    Finish,
}

/// App-side handle. Drop/kill cancels; normal completion must explicitly call finish.
pub struct Repl {
    commands: mpsc::UnboundedSender<DriverCommand>,
    events: mpsc::UnboundedReceiver<ReplEvent>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Repl {
    /// Queue code without waiting on execution, library loading, or an approval.
    pub async fn send(&mut self, code: &str, prompt_tokens: TokenTotal) -> anyhow::Result<()> {
        self.commands
            .send(DriverCommand::Call {
                code: code.to_owned(),
                prompt_tokens,
            })
            .map_err(|_| anyhow::anyhow!("REPL driver is no longer running"))
    }

    /// Cancel-safe receive for the app's event loop; unexpected closure is a driver failure.
    pub async fn next_event(&mut self) -> anyhow::Result<ReplEvent> {
        self.events
            .recv()
            .await
            .context("REPL driver exited before reporting completion")
    }

    /// Return the healthy worker to its pool. Cancellation of this future still aborts it.
    pub async fn finish(mut self) -> anyhow::Result<()> {
        self.commands
            .send(DriverCommand::Finish)
            .map_err(|_| anyhow::anyhow!("REPL driver stopped before ending its session"))?;
        if let Some(task) = self.task.as_mut() {
            task.await.context("joining REPL driver")??;
        }
        self.task = None;
        Ok(())
    }

    /// Abort drops both the checkout and any active command process-group guard.
    pub fn kill(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Drop for Repl {
    /// A turn abandoned by Esc, exit, or an error must never leave execution detached.
    fn drop(&mut self) {
        self.kill();
    }
}

struct Driver {
    pool: Arc<Pool>,
    config: Arc<ReplConfig>,
    checkout: Option<Checkout>,
    host: HostContext,
    mount: MountSpec,
    memory_mb: usize,
}

impl Driver {
    /// Serialize calls and keep the checkout owned by this abortable task throughout.
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<DriverCommand>,
    ) -> anyhow::Result<()> {
        while let Some(command) = commands.recv().await {
            match command {
                DriverCommand::Call {
                    code,
                    prompt_tokens,
                } => {
                    self.host.prompt_tokens = Some(prompt_tokens);
                    let ready = self.initialize().await;
                    self.host.code = code.clone();
                    let result = match ready {
                        Ok(()) => self.evaluate(&code).await,
                        Err(error) => Err(error),
                    };
                    let (value, error) = match result {
                        Ok(value) => (
                            if value.as_ref().type_name() == "NoneType" {
                                None
                            } else {
                                Some(value.py_repr())
                            },
                            None,
                        ),
                        Err(error) => (None, Some(self.report_error(error))),
                    };
                    if self
                        .host
                        .events
                        .send(ReplEvent::Done { value, error })
                        .is_err()
                    {
                        break;
                    }
                }
                DriverCommand::Finish => {
                    if let Some(checkout) = self.checkout.take() {
                        checkout.finish().await.context("finishing Monty session")?;
                    }
                    return Ok(());
                }
            }
        }
        // A disconnected app means cancellation, not normal session reuse.
        Ok(())
    }

    /// Load each library file once per fresh session; report ordinary load errors and continue.
    async fn initialize(&mut self) -> Result<(), PoolError> {
        if self.checkout.is_some() {
            return Ok(());
        }
        self.host.library.clear();
        self.checkout = Some(self.pool.checkout(&self.config).await?);
        let directory = self.host.config.dir.join("replib");
        let mut paths = match library_paths(&directory) {
            Ok(paths) => paths,
            Err(error) => {
                self.host
                    .output(format!("{} failed to load: {error}\n", directory.display()));
                return Ok(());
            }
        };
        paths.sort();
        for path in paths {
            let source = match std::fs::read_to_string(&path) {
                Ok(source) => source,
                Err(error) => {
                    self.host
                        .output(format!("{} failed to load: {error}\n", path.display()));
                    continue;
                }
            };
            self.host.code = source.clone();
            let functions = match host::library_functions(&source) {
                Ok(functions) => functions,
                Err(error) => {
                    self.host
                        .output(format!("{} failed to load: {error}\n", path.display()));
                    continue;
                }
            };
            match self.evaluate(&source).await {
                Ok(_) => self.host.library.extend(functions),
                Err(error) if session_lost(&error) => return Err(error),
                Err(error) => {
                    let error = self.report_error(error);
                    self.host
                        .output(format!("{} failed to load:\n{error}\n", path.display()));
                }
            }
        }
        Ok(())
    }

    /// Drive suspensions explicitly so no filesystem or command capability bypasses the host.
    async fn evaluate(&mut self, code: &str) -> Result<MontyObject, PoolError> {
        let checkout = self
            .checkout
            .as_mut()
            .expect("initialized before evaluating");
        let events = self.host.events.clone();
        let mut on_print = on_print_sync(move |_, text| {
            let _ = events.send(ReplEvent::Output(text.to_owned()));
        });
        let mut event = checkout
            .feed(
                code,
                NamedValues::new(),
                vec![self.mount.clone()],
                false,
                &mut on_print,
            )
            .await?;
        loop {
            event = match event {
                TurnEvent::Complete(value) => return Ok(value),
                TurnEvent::NameLookup {
                    name, object_id, ..
                } => {
                    let result = if object_id.is_none() && host::is_host_function(&name) {
                        NameLookupResult::Value(MontyObject::function(name, None))
                    } else {
                        NameLookupResult::Undefined
                    };
                    checkout.resume_name_lookup(result, &mut on_print).await?
                }
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    object_id,
                    call_id,
                    ..
                } => {
                    let result = if object_id.is_none() {
                        host::dispatch(&function_name, &args, &mut self.host).await
                    } else {
                        ResumeValue::NotFound
                    };
                    checkout
                        .resume(
                            checked_reply(result, call_id, &function_name),
                            &mut on_print,
                        )
                        .await?
                }
                TurnEvent::OsCall {
                    function_name,
                    args,
                    call_id,
                    ..
                } => match hostfs::handle(&function_name, &args, &self.host) {
                    FsAction::Reply(result) => {
                        checkout
                            .resume(
                                checked_reply(result, call_id, &function_name),
                                &mut on_print,
                            )
                            .await?
                    }
                    FsAction::Mounts => match checkout.resume_from_mounts(&mut on_print).await? {
                        Some(event) => event,
                        None => {
                            checkout
                                .resume(ResumeValue::NotHandled, &mut on_print)
                                .await?
                        }
                    },
                },
                // Host functions are synchronous from Python's perspective: no branch
                // registers a Future, so pending external futures indicate a broken protocol.
                TurnEvent::ResolveFutures { .. } => {
                    return Err(PoolError::Protocol(
                        "unexpected external futures: no host function registered one".into(),
                    ));
                }
            };
        }
    }

    /// Preserve ordinary Python errors; discard lost sessions without replaying the failed code.
    fn report_error(&mut self, error: PoolError) -> String {
        match &error {
            PoolError::Typing(message) => self.host.failure("type check", message.clone()),
            PoolError::Runtime(exception)
                if exception.exc_type() == ExcType::NotImplementedError =>
            {
                self.host.failure("unsupported", exception.summary());
            }
            _ => {}
        }
        if session_lost(&error) {
            self.checkout = None;
            self.host.library.clear();
            let cause = if memory_limit(&error) {
                format!("REPL memory limit of {} MiB exceeded", self.memory_mb)
            } else {
                format!("REPL worker crashed: {error}")
            };
            format!(
                "[{cause}. The REPL was restarted: every variable, import, and function defined earlier in this turn is gone; library functions are reloaded.]"
            )
        } else {
            // Expected failures carry Python frames too; report the cause without
            // treating their presence or exception type as evidence of a bug.
            match error {
                PoolError::Runtime(exception) => exception.summary(),
                other => other.to_string(),
            }
        }
    }
}

/// Monty 1.0.0 reports soft-budget and allocator failures as MemoryError text,
/// without a structured resource-error tag. Both require discarding the session.
fn memory_limit(error: &PoolError) -> bool {
    matches!(error, PoolError::Runtime(exception)
        if exception.exc_type() == ExcType::MemoryError
        && exception.message().is_some_and(|message|
            message == "the worker exceeded its memory limit and was terminated"
            || message.starts_with("memory limit exceeded: ")))
}

/// Only typing rejections and ordinary Python exceptions preserve a usable checkout.
fn session_lost(error: &PoolError) -> bool {
    memory_limit(error) || !matches!(error, PoolError::Typing(_) | PoolError::Runtime(_))
}

/// Reject an oversized answer while the suspension is still answerable with a small error.
/// Monty 1.0.0 exposes no public pending-state check after resume fails. Match its
/// ResumeCall envelope and use its size calculator, never retry an executed operation.
fn checked_reply(value: ResumeValue, call_id: u32, function_name: &str) -> ResumeValue {
    use monty_proto::{exceeds_max_frame_len, ext_result_to_proto, pb};
    use monty_types::ExtFunctionResult;

    let result = match &value {
        ResumeValue::Return(value) => ExtFunctionResult::Return(value.clone()),
        ResumeValue::Error(error) => ExtFunctionResult::Error(error.clone()),
        ResumeValue::NotFound => ExtFunctionResult::NotFound(function_name.to_owned()),
        // These carry no variable-sized payload and always fit the protocol frame.
        ResumeValue::NotHandled | ResumeValue::Future => return value,
    };
    let (result, values) = ext_result_to_proto(result);
    let request = pb::ParentRequest {
        kind: Some(pb::parent_request::Kind::ResumeCall(pb::ResumeCall {
            call_id,
            result: Some(result),
            values,
        })),
        trace_parent: None,
    };
    match exceeds_max_frame_len(&request) {
        Some(len) => ResumeValue::Error(host::exception(
            ExcType::RuntimeError,
            format!(
                "host reply frame of {len} bytes exceeds the maximum of {} bytes; the host operation is not retried and any completed effects remain",
                monty_proto::MAX_FRAME_LEN
            ),
        )),
        None => value,
    }
}

/// Enumerate optional library files; all errors other than an absent directory remain visible.
fn library_paths(directory: &std::path::Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => bail!("reading library directory {}: {error}", directory.display()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.context("reading library directory entry")?.path();
        if path.extension().is_some_and(|extension| extension == "py") {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monty_types::{CodeLoc, MontyException, StackFrame};

    /// Exercise the reporting boundary with frames, including its failure-log event.
    #[tokio::test]
    async fn reports_exception_causes_without_tracebacks() {
        // Reporting needs neither a running interpreter nor live harness settings.
        let mut pool_config = PoolConfig::subprocess(std::env::current_exe().unwrap());
        pool_config.min_processes = 0;
        pool_config.max_processes = 1;
        let pool = Arc::new(Pool::new(pool_config).await.unwrap());
        let repo = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (events, mut output) = mpsc::unbounded_channel();
        let (_, permissions) = watch::channel(Permission::None);
        let mut driver = Driver {
            pool: pool.clone(),
            config: Arc::new(ReplConfig::default()),
            checkout: None,
            mount: MountSpec::new(repo.to_str().unwrap(), &repo, MountSpecMode::ReadWrite).unwrap(),
            memory_mb: 64,
            host: HostContext {
                repo: repo.clone(),
                config: Arc::new(HostConfig {
                    dir: repo,
                    model: String::new(),
                    output_limit: 1000,
                    deny: Vec::new(),
                    allow: BTreeMap::new(),
                    environment: Vec::new(),
                    search_path: Vec::new(),
                }),
                permissions,
                events,
                library: BTreeMap::new(),
                prompt_tokens: None,
                code: "failing_call()".into(),
            },
        };
        for (kind, message, expected) in [
            (
                ExcType::FileNotFoundError,
                Some("read missing.py: file not found"),
                "FileNotFoundError: read missing.py: file not found",
            ),
            (
                ExcType::PermissionError,
                Some("write file.py: denied: the permission level is read-only"),
                "PermissionError: write file.py: denied: the permission level is read-only",
            ),
            (
                ExcType::TypeError,
                Some("run requires argv: list[str]"),
                "TypeError: run requires argv: list[str]",
            ),
            (
                ExcType::NotImplementedError,
                Some("operation is unsupported"),
                "NotImplementedError: operation is unsupported",
            ),
            (ExcType::ValueError, None, "ValueError"),
        ] {
            let exception = MontyException::with_traceback(
                kind,
                message.map(str::to_owned),
                vec![StackFrame {
                    filename: "test.py".into(),
                    start: CodeLoc::default(),
                    end: CodeLoc::default(),
                    frame_name: Some("failing_call".into()),
                    preview_line: None,
                    hide_caret: true,
                    hide_frame_name: false,
                }],
            );
            assert!(exception.to_string().contains("Traceback"));
            assert_eq!(driver.report_error(PoolError::Runtime(exception)), expected);
            if kind == ExcType::NotImplementedError {
                match output.try_recv().unwrap() {
                    ReplEvent::Failure {
                        kind,
                        message,
                        code,
                    } => {
                        assert_eq!(kind, "unsupported");
                        assert_eq!(message, expected);
                        assert_eq!(code, "failing_call()");
                    }
                    _ => panic!("expected an unsupported-operation failure record"),
                }
            }
            assert!(output.try_recv().is_err());
        }
        pool.close().await;
    }
}
