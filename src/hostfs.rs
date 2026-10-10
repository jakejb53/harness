//! Permission checks before Monty services filesystem operations through its mounts.

use std::path::Path;

use monty_pool::ResumeValue;
use monty_types::{
    CallArgs, ExcType, FileMode, MontyException, MontyObject,
    unstable::{self, MontyNode},
};

use crate::{config::Permission, host::HostContext, root::Root};

/// Virtual path of the store mount, read-write regardless of the permission level.
pub const STORE: &str = "/store";

/// Virtual path of the library mount, read-only.
pub const LIBRARY: &str = "/library";

/// Filesystem execution belongs exclusively to the feed's mounts.
pub enum FsAction {
    Mounts,
    Reply(ResumeValue),
}

/// Check each operation against the current permission without retaining grants.
pub fn handle(name: &str, args: &CallArgs, host: &HostContext) -> FsAction {
    match dispatch(name, args, host) {
        Ok(action) => action,
        Err(error) => FsAction::Reply(ResumeValue::Error(error)),
    }
}

/// Keep the environment empty and send recognized filesystem calls through mounts.
fn dispatch(name: &str, args: &CallArgs, host: &HostContext) -> Result<FsAction, MontyException> {
    match name {
        "os.getenv" => {
            return Ok(FsAction::Reply(ResumeValue::Return(
                args.arg(1).map_or_else(MontyObject::none, |v| v.to_owned()),
            )));
        }
        "os.environ" => {
            return Ok(FsAction::Reply(ResumeValue::Return(MontyObject::dict([]))));
        }
        _ => {}
    }
    let write = match name {
        "Path.write_text" | "Path.write_bytes" | "Path.append_text" | "Path.append_bytes"
        | "Path.mkdir" | "Path.unlink" | "Path.rmdir" | "Path.rename" => true,
        "open" => open_mode(args)?.create(),
        "Path.exists" | "Path.is_file" | "Path.is_dir" | "Path.is_symlink" | "Path.read_text"
        | "Path.read_bytes" | "Path.stat" | "Path.iterdir" | "Path.resolve" | "Path.absolute" => {
            false
        }
        _ => return Ok(FsAction::Reply(ResumeValue::NotHandled)),
    };
    authorize(path_arg(args, 0)?, write, host)?;
    if name == "Path.rename" {
        // Both endpoints must be covered before the mount can perform a rename. Each is
        // authorized under its own root; Monty's mount table refuses a rename whose
        // endpoints fall under different mounts.
        authorize(path_arg(args, 1)?, true, host)?;
    }
    Ok(FsAction::Mounts)
}

/// Reject uncovered paths and disallowed operations without performing host I/O.
/// The repository follows the live permission level; the store allows everything;
/// the library allows reads only; anything else is denied.
fn authorize(path: &Path, write: bool, host: &HostContext) -> Result<(), MontyException> {
    let kind = if write { "write" } else { "read" };
    // CallArgs carries Monty's normalized spelling. This prefix check only
    // rejects inputs outside mount coverage; the mount checks the original call
    // and is authoritative for symlink and traversal confinement.
    let Some(root) = root_of(path, host) else {
        return Err(host.denied(format!(
            "{kind} {}: denied: the path is outside {}",
            path.display(),
            covered(host)
        )));
    };
    match root {
        Root::Store => {}
        Root::Library if write => {
            return Err(host.denied(format!(
                "{kind} {}: denied: {LIBRARY} is read-only",
                path.display()
            )));
        }
        Root::Library => {}
        Root::Repository => {
            let permission = host.permission();
            if permission == Permission::None || (write && permission == Permission::ReadOnly) {
                return Err(host.denied(format!(
                    "{kind} {}: denied: the permission level is {}",
                    path.display(),
                    permission.as_str()
                )));
            }
        }
    }
    Ok(())
}

/// The root of the mount that serves `path` among the mounts this turn's feeds carry.
/// The longest matching virtual path wins, as in Monty's mount table, so the rule
/// applied is always that of the mount that performs the operation.
fn root_of(path: &Path, host: &HostContext) -> Option<Root> {
    [
        Some((host.repo.as_path(), Root::Repository)),
        host.mounts.store.then(|| (Path::new(STORE), Root::Store)),
        host.mounts
            .library
            .then(|| (Path::new(LIBRARY), Root::Library)),
    ]
    .into_iter()
    .flatten()
    .filter(|(prefix, _)| path.starts_with(prefix))
    .max_by_key(|(prefix, _)| prefix.as_os_str().len())
    .map(|(_, root)| root)
}

/// The places this turn's feeds cover, for the denial of a path outside all of them.
fn covered(host: &HostContext) -> String {
    match (host.mounts.store, host.mounts.library) {
        (false, false) => "the repository".into(),
        (true, false) => format!("the repository and {STORE}"),
        (false, true) => format!("the repository and {LIBRARY}"),
        (true, true) => format!("the repository, {STORE}, and {LIBRARY}"),
    }
}

/// Extract Monty's normalized path node without accepting arbitrary Python values.
fn path_arg(args: &CallArgs, index: usize) -> Result<&Path, MontyException> {
    match args.arg(index).map(unstable::node) {
        Some(MontyNode::Path(path)) => Ok(Path::new(path)),
        _ => Err(MontyException::new(
            ExcType::TypeError,
            Some(format!("filesystem argument {index}: expected a path")),
        )),
    }
}

/// Classify open-time effects from the mode Monty supplies as a string.
fn open_mode(args: &CallArgs) -> Result<FileMode, MontyException> {
    let mode = args
        .arg(1)
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            MontyException::new(
                ExcType::TypeError,
                Some("open: expected a mode string".into()),
            )
        })?;
    mode.parse()
        .map_err(|error: std::borrow::Cow<'static, str>| {
            MontyException::new(ExcType::ValueError, Some(format!("open: {error}")))
        })
}
