//! Host capabilities available to sandbox code. The driver owns this context for a turn.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cap_std::fs::Dir;
use monty_pool::ResumeValue;
use monty_types::{CallArgs, ExcType, MontyException, MontyObject, ObjectRef};
use ruff_python_ast::{Expr, Stmt};
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::{Permission, Settings};
use crate::hostfs::{self, LIBRARY, STORE};
use crate::list;
use crate::repl::ReplEvent;
use crate::root::Root;
use crate::token_count::TokenTotal;
use crate::unsupported;

/// Signatures shown by help(); dispatch validates host arguments at runtime. The mount
/// summary signatures below follow these only where their mount is present.
pub const STUBS: &str = r#"
from typing import Any
def FYI() -> None: ...
def help() -> None: ...
def run(argv: list[str], cwd: str | None = None) -> dict[str, Any]: ...
def lib_source(name: str) -> str: ...
"#;

/// `store_summary()`'s signature, listed by help() where `/store` is mounted.
const STORE_SUMMARY_STUB: &str = "def store_summary() -> None: ...";

/// `library_summary()`'s signature, listed by help() where `/library` is mounted.
const LIBRARY_SUMMARY_STUB: &str = "def library_summary() -> None: ...";

/// help()'s `Python:` section up to its `Use instead` lines, which help() generates from
/// the unsupported-name table (`unsupported::help_lines`) and follows with
/// `PYTHON_HELP_TAIL`. The modules and language gaps are fixed by the pinned Monty crates,
/// so this text must change with them. It names no replib helper; the table names only
/// the shipped `glob()`.
const PYTHON_HELP_HEAD: &str = r#"
Python:
The REPL runs a subset of Python. Importable modules: asyncio, base64, binascii, collections, copy, dataclasses, datetime, functools, itertools, json, math, os, pathlib, random, re, sys, time, typing, unicodedata. Any other import, including subprocess, glob, shutil, fnmatch, io, tempfile, and csv, raises ModuleNotFoundError.
Not available: generator functions (yield), del, match, class inheritance, method decorators.
Use instead:
"#;

/// The rest of help()'s `Python:` section, after its generated `Use instead` lines. The
/// callback rule is fixed by the pinned Monty crates and the environment line by
/// `hostfs`, so this text must change with them.
const PYTHON_HELP_TAIL: &str = r#"Calls that touch files or run commands fail inside sorted/min/max key= and map()/filter(); build a list with a comprehension first.
os.environ is empty and os.getenv() returns its default.
Write repository paths relative to the repository root. read_file, grep, list, and apply_patch are tools, not Python functions.
"#;

/// The fixed part of help()'s `Commands:` section, which states `run`'s contract; help()
/// follows it with the configured allow and deny names. Each line must stay true to
/// `run` and `arguments`: a command name or a relative path to a repository executable,
/// `argv` and `cwd` only.
const COMMANDS_HELP: &str = r#"
Commands:
Call run() inside REPL code; a tool call named run fails.
    r = run(["cargo", "test"], cwd="tools")
    print(r["exit_code"], r["stdout"], r["stderr"])
- argv: a list of strings, passed to the program as-is with no shell: no pipes, redirection, wildcards, ~, or $VARS. For shell syntax, pass ["sh", "-c", "<script>"].
- argv[0]: a command name, resolved through the lists below and then PATH; or a relative path containing / to an executable inside the repository, resolved against cwd ("target/debug/app", or "./app" with cwd="target/debug").
- cwd: a directory relative to the repository root (the default). Absolute paths and .. are rejected. os.chdir() does not affect it.
- No other arguments: no capture_output, text, timeout, env, check, or input. Output is always captured and returned; run() prints nothing.
- Returns {"exit_code": int, "stdout": str, "stderr": str}; a signal exit is negative. There is no stdin and no timeout.
- Commands are host processes: the file permission level does not apply to them, and the virtual /repository, /store, and /library paths do not exist for them.
"#;

/// The strftime format of every local date host functions print: `FYI()`'s `Date` and
/// a mount summary's last update.
const DATE_FORMAT: &str = "%a %b %d %H:%M:%S %:z %Y";

/// The most recently modified paths a mount summary lists.
const RECENT_PATHS: usize = 5;

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
    /// Canonical host path of the repository, for `run()`, which resolves command
    /// directories and repository executables on the host. Sandbox paths never name it:
    /// the repository is mounted at `hostfs::REPOSITORY`, which `hostfs` authorizes.
    pub repo: PathBuf,
    pub mounts: Mounts,
    /// The directory behind `/store`, held exactly where `mounts` has the store, for
    /// `store_summary()`, which reads it directly rather than through a feed.
    pub store_dir: Option<Arc<Dir>>,
    /// The directory behind `/library` (the agent's own library capability), held
    /// exactly where `mounts` has the library, for `library_summary()`.
    pub library_dir: Option<Arc<Dir>>,
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

/// Only these names may become host function objects during sandbox name lookup, and
/// only these are dispatched. A mount's summary function exists only where the mount
/// does, so where a mount is absent its function is absent too, like the mount.
pub fn is_host_function(name: &str, mounts: Mounts) -> bool {
    match name {
        "FYI" | "help" | "run" | "lib_source" => true,
        "store_summary" => mounts.store,
        "library_summary" => mounts.library,
        _ => false,
    }
}

/// Host function `name`'s stub line as help() lists it, without its `: ...` body, e.g.
/// `def help() -> None`; `None` where `name` is not a host function with `mounts`
/// (`is_host_function`). The stubs above stay the one source of these signatures.
pub fn signature(name: &str, mounts: Mounts) -> Option<&'static str> {
    if !is_host_function(name, mounts) {
        return None;
    }
    let start = format!("def {name}(");
    STUBS
        .lines()
        .chain([STORE_SUMMARY_STUB, LIBRARY_SUMMARY_STUB])
        .find(|line| line.starts_with(&start))
        .map(|line| line.strip_suffix(": ...").unwrap_or(line))
}

/// Helper function `name`'s signature line as help() lists it: the name followed by its
/// source parameter list, verbatim, e.g. `reasoning(text: str)`. help() and the REPL's
/// published helper signatures (`Repl::helper_signature`) both use this one format.
pub fn helper_signature(name: &str, function: &LibraryFunction) -> String {
    format!("{name}{}", function.signature)
}

/// Answer one host suspension; argument evaluation has already followed normal permissions.
pub async fn dispatch(name: &str, args: &CallArgs, host: &mut HostContext) -> ResumeValue {
    // Name lookup applies the same check; repeating it keeps an absent mount's summary
    // function undispatchable however its call was reached.
    if !is_host_function(name, host.mounts) {
        return ResumeValue::NotFound;
    }
    let result = match name {
        "FYI" => fyi(args, host),
        "help" => help(args, host),
        "run" => run(args, host).await,
        "lib_source" => lib_source(args, host),
        "store_summary" => summary(args, host, "store_summary", STORE, host.store_dir.as_ref()),
        "library_summary" => summary(
            args,
            host,
            "library_summary",
            LIBRARY,
            host.library_dir.as_ref(),
        ),
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
    let mut text = format!("Date: {}\n", chrono::Local::now().format(DATE_FORMAT));
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

/// Describe the REPL to the model: its files, the Python subset it runs, host functions
/// from their authoritative stubs, `run`'s contract with the configured allow and deny
/// names, and helper functions from their source metadata.
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
    text.push_str(PYTHON_HELP_HEAD);
    text.push_str(&unsupported::help_lines());
    text.push_str(PYTHON_HELP_TAIL);
    text.push_str("\nREPL functions (call these inside REPL code; they are not tools):\n");
    text.push_str(STUBS.trim());
    // Like the mounts above, each mount's summary function is listed only where the
    // feeds carry the mount (`is_host_function`).
    for (mounted, stub) in [
        (host.mounts.store, STORE_SUMMARY_STUB),
        (host.mounts.library, LIBRARY_SUMMARY_STUB),
    ] {
        if mounted {
            text.push('\n');
            text.push_str(stub);
        }
    }
    text.push_str("\nlib_source: reads replib/<name>.py.\n");
    text.push_str(COMMANDS_HELP);
    // Comma-joined names, with an explicit empty state like the helper listing below.
    let listing = |names: Vec<&str>| {
        if names.is_empty() {
            "(none)".to_owned()
        } else {
            names.join(", ")
        }
    };
    // `run` checks deny before allow, so a name in both lists is listed only as denied.
    let allowed = host
        .config
        .allow
        .keys()
        .filter(|name| !host.config.deny.contains(name))
        .map(String::as_str)
        .collect();
    let denied = host.config.deny.iter().map(String::as_str).collect();
    text.push_str(&format!("Run without approval: {}\n", listing(allowed)));
    text.push_str(&format!("Always denied: {}\n", listing(denied)));
    text.push_str("Any other command on PATH, and any repository executable, asks the user for approval on every call; a refusal raises PermissionError.\n");
    text.push_str("\nHelper functions:\n");
    // Explicit empty states distinguish complete listings from truncated output.
    if host.library.is_empty() {
        text.push_str("No helper functions loaded.\n");
    }
    for (name, function) in &host.library {
        text.push_str(&format!(
            "{}\n{}\n",
            helper_signature(name, function),
            function.docstring
        ));
    }
    host.output(text);
    Ok(MontyObject::none())
}

/// `store_summary()` and `library_summary()` (`name`): print the summary block of the
/// mount at `mount`, read through its directory capability `dir`. The read is first
/// authorized like any REPL read under that mount (`hostfs::authorize`), so it follows
/// the mount's rules: the store and the library are both readable at every level.
fn summary(
    args: &CallArgs,
    host: &HostContext,
    name: &str,
    mount: &str,
    dir: Option<&Arc<Dir>>,
) -> Result<MontyObject, MontyException> {
    arguments(args, &[])?;
    hostfs::authorize(Path::new(mount), false, host)?;
    // `Runtime::start` gives the capability wherever `mounts` has the mount, which the
    // authorization above required, so a missing one is a harness bug.
    let dir = dir.ok_or_else(|| {
        exception(
            ExcType::RuntimeError,
            format!("{name}: {mount} has no directory capability"),
        )
    })?;
    let mut tally = Tally {
        name,
        mount,
        bytes: 0,
        files: Vec::new(),
    };
    tally.walk(dir, Path::new(""))?;
    host.output(tally.block()?);
    Ok(MontyObject::none())
}

/// One mount's regular files, gathered for its summary block (`summary`).
struct Tally<'a> {
    /// The summary function and the mount's virtual path, which errors name.
    name: &'a str,
    mount: &'a str,
    /// The files' total size. `u128` because sparse files can each report a size near
    /// `i64::MAX`, so a `u64` sum of a few could overflow.
    bytes: u128,
    /// Each file's mtime and mount-relative path, in walk order.
    files: Vec<(SystemTime, PathBuf)>,
}

impl Tally<'_> {
    /// Add every regular file under `dir`, whose mount-relative path is `path`, hidden
    /// ones included. Entries are inspected without following symlinks, and directories
    /// are entered through `list::open_child_directory`, which refuses a symlink swapped
    /// in after the inspection, so symlinks are never followed or counted; special files
    /// are not counted either. The first failure ends the walk as the Python error.
    fn walk(&mut self, dir: &Dir, path: &Path) -> Result<(), MontyException> {
        let entries = dir.entries().map_err(|error| self.error(path, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| self.error(path, error))?;
            let name = entry.file_name();
            let child = path.join(&name);
            let metadata = entry
                .metadata()
                .map_err(|error| self.error(&child, error))?;
            if metadata.is_file() {
                let modified = metadata
                    .modified()
                    .map_err(|error| self.error(&child, error))?;
                self.bytes += u128::from(metadata.len());
                self.files.push((modified.into_std(), child));
            } else if metadata.is_dir() {
                let directory = list::open_child_directory(dir, &name)
                    .map_err(|error| self.error(&child, error))?;
                self.walk(&directory, &child)?;
            }
        }
        Ok(())
    }

    /// The Python error for a failed read at mount-relative `path`, naming the summary
    /// function and the virtual path, never the host path behind the mount.
    fn error(&self, path: &Path, error: std::io::Error) -> MontyException {
        let mut virtual_path = PathBuf::from(self.mount);
        // Pushing an empty path would append a separator to the mount itself.
        if !path.as_os_str().is_empty() {
            virtual_path.push(path);
        }
        io_error(&format!("{}: read", self.name), &virtual_path, error)
    }

    /// The block `summary` prints: `<mount>: empty` without regular files; otherwise
    /// the file count, total bytes, and newest mtime in local time, then, indented, up
    /// to `RECENT_PATHS` mount-relative paths, newest mtime first.
    fn block(mut self) -> Result<String, MontyException> {
        if self.files.is_empty() {
            return Ok(format!("{}: empty\n", self.mount));
        }
        // Newest first; equal mtimes fall back to ascending path so the output does not
        // depend on the filesystem's walk order.
        self.files.sort_by(|(a_time, a_path), (b_time, b_path)| {
            b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
        });
        let (newest, newest_path) = &self.files[0];
        let updated = local_time(*newest).ok_or_else(|| {
            self.error(
                newest_path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "modification time is outside the supported date range",
                ),
            )
        })?;
        let recent: Vec<String> = self
            .files
            .iter()
            .take(RECENT_PATHS)
            .map(|(_, path)| path.display().to_string())
            .collect();
        Ok(format!(
            "{}: {} files, {} bytes, last updated {}\n  Recent: {}\n",
            self.mount,
            self.files.len(),
            self.bytes,
            updated.format(DATE_FORMAT),
            recent.join(", ")
        ))
    }
}

/// `time` as a local date, or `None` outside chrono's range: an mtime can be set to
/// any value, and chrono's own `SystemTime` conversion panics outside it.
fn local_time(time: SystemTime) -> Option<chrono::DateTime<chrono::Local>> {
    let utc = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => chrono::DateTime::UNIX_EPOCH
            .checked_add_signed(chrono::TimeDelta::from_std(after).ok()?),
        Err(before) => chrono::DateTime::UNIX_EPOCH
            .checked_sub_signed(chrono::TimeDelta::from_std(before.duration()).ok()?),
    }?;
    Some(utc.with_timezone(&chrono::Local))
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
    // The working directory is resolved before the command, because a path-form command
    // resolves against it.
    let cwd = match argument(args, 1, "cwd") {
        None => host.repo.clone(),
        Some(value) if value.type_name() == "NoneType" => host.repo.clone(),
        Some(value) => {
            let value = value
                .as_str()
                .ok_or_else(|| exception(ExcType::TypeError, "run cwd must be a string or None"))?;
            require_relative(host, &format!("run cwd {value:?}"), value)?;
            host.repo.join(value)
        }
    };
    // Commands require an existing working directory; unlike file creation,
    // there is no missing-path suffix to resolve here.
    let cwd = cwd
        .canonicalize()
        .map_err(|error| io_error("resolve command directory", &cwd, error))?;
    // `label` is what errors after resolution name: a path-form command by its spelling,
    // never the host path behind it; a bare name by its resolved executable.
    let (executable, label, configured) = if name.contains('/') {
        // `deny` holds bare names matched exactly and `allow` is never consulted for a
        // path, so a repository executable always reaches the command prompt below.
        let executable = repository_executable(name, &cwd, host)?;
        (executable, PathBuf::from(name), None)
    } else {
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
        (executable.clone(), executable, configured)
    };
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
        return Err(host.denied(format!("run '{}': denied by the user", label.display())));
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
        .map_err(|error| io_error("run", &label, error))?;
    let mut group = CommandGroup(child.id().map(|id| id as libc::pid_t));
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| io_error("wait for", &label, error))?;
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
            Ok(metadata) if is_executable(&metadata) => {
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

/// Resolve `spelled`, a path-form `argv[0]`, to the canonical path of an executable
/// inside the repository, joined to the canonical command directory `cwd`. The spelling
/// is checked before the join and the canonical result must lie under the repository,
/// so neither `..` nor a symlink reaches outside it. Errors name the spelling, never the
/// host path behind it.
fn repository_executable(
    spelled: &str,
    cwd: &Path,
    host: &HostContext,
) -> Result<PathBuf, MontyException> {
    require_relative(host, &format!("run '{spelled}'"), spelled)?;
    let executable = cwd
        .join(spelled)
        .canonicalize()
        .map_err(|error| io_error("resolve command", Path::new(spelled), error))?;
    if !executable.starts_with(&host.repo) {
        return Err(host.denied(format!(
            "run '{spelled}': denied: resolves outside the repository"
        )));
    }
    let metadata = executable
        .metadata()
        .map_err(|error| io_error("resolve command", Path::new(spelled), error))?;
    // Not an app denial, so it is not logged as one: the OS refuses to execute such a
    // file (EACCES), and this is the PermissionError CPython's subprocess raises for it.
    if !is_executable(&metadata) {
        return Err(exception(
            ExcType::PermissionError,
            format!("run '{spelled}': not an executable file"),
        ));
    }
    Ok(executable)
}

/// Whether `metadata` is a regular file with an execute bit: the test `run` applies to
/// an executable it finds, on PATH (`resolve_command`) or in the repository
/// (`repository_executable`); configured `allow` paths are spawned as given.
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

/// Deny `spelled`, an agent-written `run` path (`cwd` or a path-form `argv[0]`), when
/// it is absolute or has a `..` component. The spelling is inspected before it is
/// joined to anything: containment alone would allow those forbidden spellings.
/// `subject` opens the denial, e.g. `run cwd "src"`.
fn require_relative(
    host: &HostContext,
    subject: &str,
    spelled: &str,
) -> Result<(), MontyException> {
    let path = Path::new(spelled);
    if path.is_absolute()
        || path
            .components()
            .any(|part| part == std::path::Component::ParentDir)
    {
        return Err(host.denied(format!(
            "{subject}: denied: absolute paths and '..' components are not allowed"
        )));
    }
    Ok(())
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
