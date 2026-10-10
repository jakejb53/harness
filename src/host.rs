//! Host capabilities available to sandbox code. The driver owns this context for a turn.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use monty_pool::ResumeValue;
use monty_types::{CallArgs, ExcType, MontyException, MontyObject, ObjectRef};
use ruff_python_ast::{Expr, Stmt};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{Permission, Settings};
use crate::repl::ReplEvent;
use crate::root::Root;
use crate::token_count::TokenTotal;

/// Signatures shown by help(); dispatch validates host arguments at runtime.
pub const STUBS: &str = r#"
from typing import Any
def FYI() -> None: ...
def help() -> None: ...
def run(argv: list[str], cwd: str | None = None) -> dict[str, Any]: ...
def lib_source(name: str) -> str: ...
"#;

/// Immutable projection of startup settings; permission changes use the watch channel.
pub struct HostConfig {
    pub dir: PathBuf,
    pub model: String,
    pub output_limit: usize,
    pub deny: Vec<String>,
    pub allow: BTreeMap<String, PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
    pub search_path: Vec<PathBuf>,
}

impl HostConfig {
    /// Capture command environment privately, filtering exact names before any spawn.
    pub fn new(settings: &Settings) -> Self {
        Self {
            dir: settings.dir.clone(),
            model: settings.endpoint.model.clone(),
            output_limit: settings.output_limit,
            deny: settings.commands.deny.clone(),
            allow: settings.commands.allow.clone(),
            environment: std::env::vars_os()
                .filter(|(key, _)| {
                    !settings
                        .commands
                        .env_filter
                        .iter()
                        .any(|name| key == name.as_str())
                })
                .collect(),
            search_path: std::env::var_os("PATH")
                .map(|value| std::env::split_paths(&value).collect())
                .unwrap_or_default(),
        }
    }
}

/// One loaded public function's source metadata; failed files never enter this listing.
pub struct LibraryFunction {
    pub signature: String,
    pub docstring: String,
}

/// The mounts beside the repository that this agent's feeds carry, fixed by its depth
/// for the whole turn. Authorization (`hostfs`) and `help()` both read it, so neither
/// can describe or allow a mount the feeds lack; the agent's direct-tool roots and
/// their definitions follow the same value (`has`, `roots`).
#[derive(Clone, Copy)]
pub struct Mounts {
    /// `/store`, read-write regardless of the permission level (depth 1 and deeper).
    pub store: bool,
    /// `/library`, read-only (depth 2 and deeper, when the App supplied it).
    pub library: bool,
}

impl Mounts {
    /// The mounts of an agent at `depth`: `/store` from depth 1, and `/library` when
    /// the App supplied the tree's library, which it does at depth 2 and deeper.
    pub fn at(depth: u32, library: bool) -> Self {
        Self {
            store: depth >= 1,
            library,
        }
    }

    /// Whether the agent reaches `root`; every agent reaches the repository.
    pub fn has(self, root: Root) -> bool {
        match root {
            Root::Repository => true,
            Root::Store => self.store,
            Root::Library => self.library,
        }
    }

    /// The roots the agent reaches, in `Root::ALL` order, the repository first.
    pub fn roots(self) -> impl Iterator<Item = Root> {
        Root::ALL.into_iter().filter(move |root| self.has(*root))
    }
}

pub struct HostContext {
    /// Canonical repository path, which is also its mount's virtual path.
    pub repo: PathBuf,
    pub mounts: Mounts,
    pub config: Arc<HostConfig>,
    pub permissions: watch::Receiver<Permission>,
    pub events: mpsc::UnboundedSender<ReplEvent>,
    pub library: BTreeMap<String, LibraryFunction>,
    pub prompt_tokens: Option<TokenTotal>,
    /// The actual fed snippet, including library source when loading fails.
    pub code: String,
}

impl HostContext {
    /// Read at the operation boundary, so Shift+Tab affects the next operation mid-call.
    pub fn permission(&self) -> Permission {
        *self.permissions.borrow()
    }

    /// Suspend the driver until this particular prompt is answered or canceled.
    pub async fn ask(
        &self,
        operation: String,
        description_input: String,
    ) -> Result<bool, MontyException> {
        let (answer, response) = oneshot::channel();
        self.events
            .send(ReplEvent::Prompt {
                operation,
                description_input,
                answer,
            })
            .map_err(|_| exception(ExcType::RuntimeError, "approval canceled: the turn ended"))?;
        response
            .await
            .map_err(|_| exception(ExcType::RuntimeError, "approval canceled: the turn ended"))
    }

    /// Log denials at their source even if sandbox code catches the raised exception.
    pub fn denied(&self, message: String) -> MontyException {
        self.failure("denied", message.clone());
        exception(ExcType::PermissionError, message)
    }

    /// Send failure facts to the app; persistence and occurrence counts belong there.
    pub fn failure(&self, kind: &'static str, message: String) {
        let _ = self.events.send(ReplEvent::Failure {
            kind,
            message,
            code: self.code.clone(),
        });
    }

    /// Host text joins the same ordered stream as Monty print events.
    pub fn output(&self, text: String) {
        let _ = self.events.send(ReplEvent::Output(text));
    }
}

/// Only these names may become host function objects during sandbox name lookup.
pub fn is_host_function(name: &str) -> bool {
    matches!(name, "FYI" | "help" | "run" | "lib_source")
}

/// Answer one host suspension; argument evaluation has already followed normal permissions.
pub async fn dispatch(name: &str, args: &CallArgs, host: &mut HostContext) -> ResumeValue {
    let result = match name {
        "FYI" => fyi(args, host),
        "help" => help(args, host),
        "run" => run(args, host).await,
        "lib_source" => lib_source(args, host),
        _ => return ResumeValue::NotFound,
    };
    match result {
        Ok(value) => ResumeValue::Return(value),
        Err(error) => ResumeValue::Error(error),
    }
}

/// Reject unknown, duplicate, and surplus arguments even when reached through an alias.
fn arguments(args: &CallArgs, names: &[&str]) -> Result<(), MontyException> {
    if args.args().len() > names.len() {
        return Err(exception(
            ExcType::TypeError,
            "too many positional arguments",
        ));
    }
    for (key, _) in args.kwargs() {
        let Some(index) = names.iter().position(|name| Some(*name) == key.as_str()) else {
            return Err(exception(
                ExcType::TypeError,
                format!("unexpected keyword argument {}", key.py_repr()),
            ));
        };
        if args.arg(index).is_some() {
            return Err(exception(
                ExcType::TypeError,
                format!("multiple values for argument '{}'", names[index]),
            ));
        }
    }
    Ok(())
}

/// Fetch a positional-or-keyword argument without losing its arena lifetime.
fn argument<'a>(args: &'a CallArgs, index: usize, name: &str) -> Option<ObjectRef<'a>> {
    args.arg(index).or_else(|| args.kwarg(name))
}

/// Keep synthetic and explicit snapshots on the same implementation.
fn fyi(args: &CallArgs, host: &HostContext) -> Result<MontyObject, MontyException> {
    arguments(args, &[])?;
    host.output(format!(
        "FYI()\n{}",
        fyi_lines(host.prompt_tokens, &host.config.model)
    ));
    Ok(MontyObject::none())
}

/// The `Date`, `Prompt tokens` (when known), and `Model` lines, shared by the REPL's
/// `FYI()` host function and the root's `FYI` tool so the two report alike.
pub fn fyi_lines(prompt_tokens: Option<TokenTotal>, model: &str) -> String {
    let mut text = format!(
        "Date: {}\n",
        chrono::Local::now().format("%a %b %d %H:%M:%S %:z %Y")
    );
    if let Some(tokens) = prompt_tokens {
        text.push_str(&prompt_tokens_line(tokens));
    }
    text.push_str(&format!("Model: {model}\n"));
    text
}

/// Preserve the overlay's estimate marker in the snapshot shown to the model.
fn prompt_tokens_line(tokens: TokenTotal) -> String {
    let estimate = if tokens.estimated { "~" } else { "" };
    format!("Prompt tokens: {estimate}{}\n", tokens.count)
}

/// Describe loaded capabilities from their authoritative stubs and source metadata.
fn help(args: &CallArgs, host: &HostContext) -> Result<MontyObject, MontyException> {
    arguments(args, &[])?;
    let mut text = format!(
        "help()\nOutput limit: {} bytes per call. Keep large data in variables or files.\n",
        host.config.output_limit
    );
    let notes_path = host.config.dir.join("repl-notes.md");
    match std::fs::read_to_string(&notes_path) {
        Ok(notes) => {
            text.push_str(&notes);
            text.push('\n');
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("read", &notes_path, error)),
    }
    // Only the mounts this agent's feeds carry are listed. The text names neither the
    // agent's depth nor the tree; the permission is read live, like every operation.
    text.push_str(&format!(
        "\nFiles:\nRepository (permission currently {}): REPL relative paths resolve here; read_file, grep, list, and apply_patch use it by default (root = \"repository\").\n",
        host.permission().as_str()
    ));
    if host.mounts.store {
        text.push_str("/store (read-write): a shared knowledge base that persists across sessions. Check it for earlier findings, save findings worth reusing, and pass large data by writing it here and naming the path instead of copying the data. REPL: pathlib.Path paths under /store; read_file, grep, list, and apply_patch: root = \"store\".\n");
    }
    if host.mounts.library {
        text.push_str("/library (read-only): session transcripts available to inspect, one JSON file per session. REPL: pathlib.Path paths under /library; read_file, grep, and list: root = \"library\".\n");
    }
    text.push_str("\nHost functions:\n");
    text.push_str(STUBS.trim());
    text.push_str("\nrun: executes a command name directly; returns exit_code, stdout, stderr.\nlib_source: reads replib/<name>.py.\n");
    text.push_str("\nLibrary functions:\n");
    // Explicit empty states distinguish complete listings from truncated output.
    if host.library.is_empty() {
        text.push_str("No library functions loaded.\n");
    }
    for (name, function) in &host.library {
        text.push_str(&format!(
            "{name}{}\n{}\n",
            function.signature, function.docstring
        ));
    }
    text.push_str("\nCommands configured to run without approval (deny takes precedence):\n");
    if host.config.allow.is_empty() {
        text.push_str("(none)\n");
    }
    for name in host.config.allow.keys() {
        text.push_str(name);
        text.push('\n');
    }
    text.push_str("\nOther commands found on PATH require approval unless denied.\n");
    host.output(text);
    Ok(MontyObject::none())
}

/// Read only a named library file; reject names that could escape its directory.
fn lib_source(args: &CallArgs, host: &HostContext) -> Result<MontyObject, MontyException> {
    arguments(args, &["name"])?;
    let name = argument(args, 0, "name")
        .and_then(|arg| arg.as_str())
        .ok_or_else(|| exception(ExcType::TypeError, "lib_source requires a string name"))?;
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        || !chars.all(|c| c == '_' || c.is_alphanumeric())
    {
        return Err(exception(
            ExcType::ValueError,
            format!("lib_source: invalid function name {name:?}"),
        ));
    }
    let path = host.config.dir.join("replib").join(format!("{name}.py"));
    std::fs::read_to_string(&path)
        .map(MontyObject::string)
        .map_err(|error| io_error("read", &path, error))
}

/// Execute directly in an isolated process group; filesystem permission does not grant commands.
async fn run(args: &CallArgs, host: &HostContext) -> Result<MontyObject, MontyException> {
    arguments(args, &["argv", "cwd"])?;
    let argv = argument(args, 0, "argv")
        .filter(|arg| arg.type_name() == "list")
        .and_then(|arg| arg.items())
        .ok_or_else(|| exception(ExcType::TypeError, "run requires argv: list[str]"))?;
    let argv: Vec<String> = argv
        .into_iter()
        .map(|arg| {
            arg.as_str().map(str::to_owned).ok_or_else(|| {
                exception(
                    ExcType::TypeError,
                    "run requires every argv item to be a string",
                )
            })
        })
        .collect::<Result<_, _>>()?;
    let name = argv
        .first()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| exception(ExcType::ValueError, "run requires a nonempty command name"))?;
    if name.contains('/') {
        return Err(exception(
            ExcType::ValueError,
            format!("run takes a command name, not a path: {name:?}"),
        ));
    }
    if host.config.deny.contains(name) {
        return Err(host.denied(format!(
            "run '{name}': denied: '{name}' is on the command deny list"
        )));
    }
    let configured = host.config.allow.get(name);
    let executable = match configured {
        Some(path) => path.clone(),
        None => resolve_command(name, &host.config.search_path)?,
    };
    let cwd = match argument(args, 1, "cwd") {
        None => host.repo.clone(),
        Some(value) if value.type_name() == "NoneType" => host.repo.clone(),
        Some(value) => {
            let value = value
                .as_str()
                .ok_or_else(|| exception(ExcType::TypeError, "run cwd must be a string or None"))?;
            // Inspect the agent's spelling before joining it to the repository:
            // containment alone would allow forbidden absolute and parent paths.
            if Path::new(value).is_absolute()
                || Path::new(value)
                    .components()
                    .any(|part| part == std::path::Component::ParentDir)
            {
                return Err(host.denied(format!(
                    "run cwd {value:?}: denied: absolute paths and '..' components are not allowed"
                )));
            }
            host.repo.join(value)
        }
    };
    // Commands require an existing working directory; unlike file creation,
    // there is no missing-path suffix to resolve here.
    let cwd = cwd
        .canonicalize()
        .map_err(|error| io_error("resolve command directory", &cwd, error))?;
    if !cwd.starts_with(&host.repo)
        && !host
            .ask(
                format!("use command directory {}", cwd.display()),
                serde_json::json!({"operation": "use_command_directory", "directory": cwd.to_string_lossy()}).to_string(),
            )
            .await?
    {
        return Err(host.denied(format!("run in {}: denied by the user", cwd.display())));
    }
    // Debug quoting exposes argument boundaries and escapes control characters in prompts.
    if configured.is_none()
        && !host
            .ask(
                format!("{} {:?}", executable.display(), &argv[1..]),
                serde_json::json!({
                    "operation": "run_command", "executable": executable.to_string_lossy(),
                    "arguments": &argv[1..], "working_directory": cwd.to_string_lossy(),
                })
                .to_string(),
            )
            .await?
    {
        return Err(host.denied(format!(
            "run '{}': denied by the user",
            executable.display()
        )));
    }
    let mut command = tokio::process::Command::new(&executable);
    command
        .args(&argv[1..])
        .current_dir(&cwd)
        .env_clear()
        .envs(host.config.environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: setsid is async-signal-safe; no locks or allocation in the pre-exec hook.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let child = command
        .spawn()
        .map_err(|error| io_error("run", &executable, error))?;
    let mut group = CommandGroup(child.id().map(|id| id as libc::pid_t));
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| io_error("wait for", &executable, error))?;
    group.0 = None;
    let exit_code = output
        .status
        .code()
        .or_else(|| output.status.signal().map(|signal| -signal))
        .ok_or_else(|| {
            exception(
                ExcType::RuntimeError,
                "command ended without an exit code or signal",
            )
        })?;
    Ok(MontyObject::dict([
        (
            MontyObject::string("exit_code"),
            MontyObject::int(i64::from(exit_code)),
        ),
        (
            MontyObject::string("stdout"),
            MontyObject::string(String::from_utf8_lossy(&output.stdout)),
        ),
        (
            MontyObject::string("stderr"),
            MontyObject::string(String::from_utf8_lossy(&output.stderr)),
        ),
    ]))
}

/// Aborting the driver drops this guard while wait_with_output owns the direct child.
struct CommandGroup(Option<libc::pid_t>);

impl Drop for CommandGroup {
    /// Kill descendants as well as the direct child when the command future is canceled.
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // SAFETY: pgid came from our child, which called setsid before exec.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

/// Resolve only executable files through the host PATH, returning an absolute prompt target.
fn resolve_command(name: &str, paths: &[PathBuf]) -> Result<PathBuf, MontyException> {
    for dir in paths {
        let candidate = dir.join(name);
        match candidate.metadata() {
            Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
                return std::path::absolute(&candidate)
                    .map_err(|error| io_error("resolve command", &candidate, error));
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(io_error("resolve command", &candidate, error)),
        }
    }
    Err(exception(
        ExcType::FileNotFoundError,
        format!("run: no command named '{name}' is configured or found on the host"),
    ))
}

/// Extract public signatures and docstrings without executing the source in the host.
pub fn library_functions(source: &str) -> Result<BTreeMap<String, LibraryFunction>, String> {
    let parsed = ruff_python_parser::parse_module(source).map_err(|error| error.to_string())?;
    let mut functions = BTreeMap::new();
    for statement in parsed.into_syntax().body {
        let Stmt::FunctionDef(function) = statement else {
            continue;
        };
        let name = function.name.id.to_string();
        if name.starts_with('_') {
            continue;
        }
        let range = function.parameters.range;
        let signature = source[usize::from(range.start())..usize::from(range.end())].to_owned();
        let docstring = match function.body.first() {
            Some(Stmt::Expr(expression)) => match expression.value.as_ref() {
                Expr::StringLiteral(literal) => literal.value.to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        functions.insert(
            name,
            LibraryFunction {
                signature,
                docstring,
            },
        );
    }
    Ok(functions)
}

/// Construct a catchable Python error with the operation's human-readable cause.
pub fn exception(kind: ExcType, message: impl Into<String>) -> MontyException {
    MontyException::new(kind, Some(message.into()))
}

/// Translate expected host I/O failures into Python errors instead of host tracebacks.
pub fn io_error(operation: &str, path: &Path, error: std::io::Error) -> MontyException {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => ExcType::FileNotFoundError,
        std::io::ErrorKind::PermissionDenied => ExcType::PermissionError,
        std::io::ErrorKind::AlreadyExists => ExcType::FileExistsError,
        std::io::ErrorKind::NotADirectory => ExcType::NotADirectoryError,
        std::io::ErrorKind::IsADirectory => ExcType::IsADirectoryError,
        _ => ExcType::OSError,
    };
    exception(kind, format!("{operation} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FYI exposes whether its overlay-derived fallback is heuristic.
    #[test]
    fn prompt_token_line_marks_estimates() {
        assert_eq!(
            prompt_tokens_line(TokenTotal {
                count: 123,
                estimated: false,
            }),
            "Prompt tokens: 123\n"
        );
        assert_eq!(
            prompt_tokens_line(TokenTotal {
                count: 123,
                estimated: true,
            }),
            "Prompt tokens: ~123\n"
        );
    }
}
