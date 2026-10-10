//! Direct patch execution: parse first, apply operations in order, and account for every commit.
//! The patch syntax follows OpenAI Codex's apply-patch format; this implementation is independent.

mod engine;
mod fs;

use std::io;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use tokio::sync::oneshot;

use engine::Operation;
use fs::{Existing, Repository, Staged, Target};

use crate::root::{Access, Root};

pub const DESCRIPTION: &str = r#"Edit files directly with literal patch text. Text updates require UTF-8; deletion and rename-only operations do not read the contents. Requires read-write permission. Paths must be repository-relative; absolute paths and escaping path resolution are rejected. Text updates follow relative symlinks within the repository. Rename and deletion operate on named entries. Add and Move destinations must not exist.
Use *** Begin Patch and *** End Patch around operations:
*** Add File: path (each content line prefixed +)
*** Replace File: path (each content line prefixed +; replaces the whole text of an existing file without restating the old text, keeping its line endings, final-newline state, and permissions)
*** Delete File: path
*** Update File: path (optionally followed by *** Move to: new-path)
Start a hunk with @@ alone. An optional anchor goes on that SAME line after "@@ ": for example, "@@ fn example() {" searches after the exact source line "fn example() {". The anchor is not part of the replacement. Omit it when the hunk's context/removal lines already identify one exact match. When the anchor text repeats, number it: "@@ line 63: fn example() {" requires line 63 of the file's current contents to be exactly that text and searches after it; a different line 63 is an error. Line numbers refer to the file before this update's hunks apply.
Prefix unchanged context with one space, removed lines with -, and added lines with +. After removing these prefixes, context/removal lines form the old text and context/addition lines form the new text, each in the order written. Put additions where they belong relative to unchanged context. For example:
*** Begin Patch
*** Update File: example.rs
@@
-    old_call();
+    new_call();
     unchanged_call();
*** End Patch
This replaces old_call before unchanged_call. Replacing @@ with @@ fn example() { adds a same-line anchor; it does not change the resulting line order.
Within one file update, hunks match its original contents and changed ranges must not overlap. Use *** End of File to constrain a hunk to the file end. An insertion without old lines or an anchor appends at EOF.
JSON escaping happens once, before patch parsing. In the JSON string, \r and \t decode to actual CR and TAB characters; \\r and \\t decode to literal backslash-r and backslash-t. Patch text is then literal: there is no shell, Python, or replacement-string interpretation. LF separates lines, CRLF is recognized, and a bare CR or TAB inside a line is data. An actual LF in old text requires a separate - line for each physical line.
To add the Rust source line:
    out.push_str("\\r");
the JSON string representing that single added patch line is:
"+    out.push_str(\"\\\\r\");"
Read files before editing. Existing line endings and final-newline state are preserved; new files use LF. The complete patch is parsed first, then operations are validated and applied in order against the current filesystem. A later failure leaves earlier changes applied; inspect the receipt before retrying."#;

/// Holds a write capability for each writable root only: the library is read-only, so
/// no capability for it exists here.
pub struct Runtime {
    /// The launch directory; writes follow `writable`.
    repository: Arc<Repository>,
    /// The store, writable at every permission level.
    store: Arc<Repository>,
    /// Whether the permission level allows repository writes.
    writable: Arc<AtomicBool>,
}

impl Runtime {
    /// Establish filesystem authority once, independently of the Monty worker lifecycle.
    pub fn new(repository: &Path, store: &Path, writable: bool) -> io::Result<Self> {
        Ok(Self {
            repository: Arc::new(Repository::open(repository, Root::Repository)?),
            store: Arc::new(Repository::open(store, Root::Store)?),
            writable: Arc::new(AtomicBool::new(writable)),
        })
    }

    /// Permission changes are observed before the next repository patch filesystem
    /// operation.
    pub fn set_permission(&self, writable: bool) {
        self.writable.store(writable, Ordering::SeqCst);
    }

    /// Keep file I/O off the UI thread while retaining a joinable, cooperatively stopped
    /// writer confined to the root `access` names. The library is refused before any
    /// file operation.
    pub fn start(&self, patch: String, access: Access) -> io::Result<Job> {
        let (repo, writable) = match access {
            Access::Repository => (self.repository.clone(), self.writable.clone()),
            // The store is writable at every permission level, so its flag is a fresh
            // constant `true`.
            Access::Store => (self.store.clone(), Arc::new(AtomicBool::new(true))),
            Access::Library(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("root {:?} is read-only", Root::Library.as_str()),
                ));
            }
        };
        let control = Control {
            canceled: Arc::new(AtomicBool::new(false)),
            writable,
        };
        control.check().map_err(io::Error::other)?;
        let worker_control = control.clone();
        let (finished, completion) = oneshot::channel();
        let worker = thread::Builder::new()
            .name("harness-patch".into())
            .spawn(move || {
                let result = execute(&repo, &patch, &worker_control);
                let _ = finished.send(());
                result
            })?;
        Ok(Job {
            control,
            worker: Some(worker),
            completion,
        })
    }
}

#[derive(Clone)]
struct Control {
    canceled: Arc<AtomicBool>,
    writable: Arc<AtomicBool>,
}

impl Control {
    /// Never start a new mutation after cancellation or revocation of write authority.
    fn check(&self) -> Result<(), String> {
        if self.canceled.load(Ordering::SeqCst) {
            return Err("stopped by user".into());
        }
        if !self.writable.load(Ordering::SeqCst) {
            return Err("apply_patch requires read-write permission".into());
        }
        Ok(())
    }
}

pub struct Job {
    control: Control,
    worker: Option<thread::JoinHandle<String>>,
    completion: oneshot::Receiver<()>,
}

impl Job {
    /// Waiting is cancel-safe; only a completed worker is joined by the event loop.
    pub async fn result(&mut self) -> String {
        let _ = (&mut self.completion).await;
        self.join()
    }

    /// A stopped tool result must include commits made before the writer acknowledged cancellation.
    pub fn stop(&mut self) -> String {
        self.control.canceled.store(true, Ordering::SeqCst);
        self.join()
    }

    /// Consume the worker exactly once, surfacing an unexpected panic without claiming rollback.
    fn join(&mut self) -> String {
        self.worker.take().expect("patch worker joined once").join().unwrap_or_else(|_| {
            "Internal patch worker failure: committed changes could not be determined; inspect affected files before retrying.".into()
        })
    }
}

impl Drop for Job {
    /// Even exceptional app exits cannot detach a writer that still has filesystem authority.
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.stop();
        }
    }
}

enum Prepared {
    Add(Target, String),
    Update(Existing, String),
    Move(Existing, Existing, Target, String),
    Rename(Existing, Target),
    Delete(Existing),
}

struct Change {
    label: String,
    view: String,
    prepared: Prepared,
}

enum Action {
    Write(Staged),
    Move(Staged, Existing, Target, String),
    Rename(Existing, Target),
    Delete(Existing),
}

struct Ready {
    label: String,
    view: String,
    action: Action,
}

/// Validate one operation against the filesystem left by earlier completed operations.
fn prepare(repo: &Repository, operation: Operation, control: &Control) -> Result<Change, String> {
    control.check()?;
    let path = match &operation {
        Operation::Add { path, .. }
        | Operation::Delete { path }
        | Operation::Update { path, .. }
        | Operation::Replace { path, .. } => path,
    };
    let path = path.clone();
    let result = (|| -> Result<Change, String> {
        match operation {
            Operation::Add { contents, .. } => {
                let target = repo.vacant(Path::new(&path)).map_err(|e| e.to_string())?;
                let view = numbered(
                    &path,
                    &contents,
                    &[(1, engine::source_lines(&contents).len().max(1))],
                );
                Ok(Change {
                    label: format!("added {path}"),
                    view,
                    prepared: Prepared::Add(target, contents),
                })
            }
            Operation::Delete { .. } => {
                let file = repo
                    .existing(Path::new(&path), false)
                    .map_err(|e| e.to_string())?;
                Ok(Change {
                    label: format!("deleted {path}"),
                    view: String::new(),
                    prepared: Prepared::Delete(file),
                })
            }
            Operation::Replace { lines, .. } => {
                // A text update without matching: the target must exist and be
                // UTF-8 like any update, and it keeps its identity, mode, and endings.
                let file = repo
                    .existing(Path::new(&path), true)
                    .map_err(|e| e.to_string())?;
                let original = std::str::from_utf8(&file.bytes)
                    .map_err(|e| format!("file is not UTF-8: {e}"))?;
                let update = engine::replace(original, &lines);
                let view = numbered(&path, &update.contents, &update.regions);
                Ok(Change {
                    label: format!("replaced {path}"),
                    view,
                    prepared: Prepared::Update(file, update.contents),
                })
            }
            Operation::Update {
                move_to, chunks, ..
            } => {
                // A rename without a text edit is a filesystem operation: do
                // not require read permission, UTF-8, or dereference a final symlink.
                if chunks.is_empty()
                    && let Some(destination) = &move_to
                {
                    let source = repo
                        .existing(Path::new(&path), false)
                        .map_err(|e| e.to_string())?;
                    let target = repo
                        .vacant(Path::new(destination))
                        .map_err(|e| format!("destination {destination}: {e}"))?;
                    return Ok(Change {
                        label: format!("moved {path} -> {destination}"),
                        view: String::new(),
                        prepared: Prepared::Rename(source, target),
                    });
                }
                let file = repo
                    .existing(Path::new(&path), true)
                    .map_err(|e| e.to_string())?;
                let original = std::str::from_utf8(&file.bytes)
                    .map_err(|e| format!("file is not UTF-8: {e}"))?;
                let update = engine::apply(original, &chunks)?;
                match move_to {
                    Some(destination) => {
                        control.check()?;
                        let target = repo
                            .vacant(Path::new(&destination))
                            .map_err(|e| format!("destination {destination}: {e}"))?;
                        // Reading/editing follows the final link; rename acts
                        // on the named entry, exactly like the corresponding OS operations.
                        let source = repo
                            .existing(Path::new(&path), false)
                            .map_err(|e| e.to_string())?;
                        // Moving a relative link can make it dangling. Its edited
                        // target's contents belong under the actual target path.
                        let edited_path = file.target.canonical_path.display().to_string();
                        let view_path = if source.is_symlink {
                            &edited_path
                        } else {
                            &destination
                        };
                        let view = numbered(view_path, &update.contents, &update.regions);
                        let label = if source.is_symlink {
                            format!("updated {edited_path}; moved {path} -> {destination}")
                        } else {
                            format!("moved {path} -> {destination}")
                        };
                        Ok(Change {
                            label,
                            view,
                            prepared: Prepared::Move(file, source, target, update.contents),
                        })
                    }
                    None => {
                        let view = numbered(&path, &update.contents, &update.regions);
                        Ok(Change {
                            label: format!("updated {path}"),
                            view,
                            prepared: Prepared::Update(file, update.contents),
                        })
                    }
                }
            }
        }
    })();
    result.map_err(|error| format!("{path}: {error}"))
}

/// Stage only the current operation, leaving future operations to observe the real filesystem.
fn stage(change: Change, control: &Control) -> Result<Ready, String> {
    let Change {
        label,
        view,
        prepared,
    } = change;
    let action = control.check().and_then(|()| {
        match prepared {
            Prepared::Add(target, contents) => target.stage(contents.as_bytes()).map(Action::Write),
            Prepared::Update(file, contents) => file.stage(contents.as_bytes()).map(Action::Write),
            Prepared::Move(file, source, target, contents) => {
                let edited_path = file.target.canonical_path.display().to_string();
                file.stage(contents.as_bytes())
                    .map(|staged| Action::Move(staged, source, target, edited_path))
            }
            Prepared::Rename(source, target) => Ok(Action::Rename(source, target)),
            Prepared::Delete(file) => Ok(Action::Delete(file)),
        }
        .map_err(|error| format!("staging {label}: {error}"))
    })?;
    Ok(Ready {
        label,
        view,
        action,
    })
}

/// Normal failure and cancellation must report temporary-file cleanup errors, not hide them in Drop.
fn cleanup(ready: &mut [Ready]) -> Vec<String> {
    let mut errors = Vec::new();
    for item in ready {
        let result = match &mut item.action {
            Action::Write(staged) | Action::Move(staged, ..) => staged.cleanup(),
            Action::Rename(..) | Action::Delete(_) => Ok(()),
        };
        if let Err(error) = result {
            errors.push(format!("cleanup for {}: {error}", item.label));
        }
    }
    errors
}

/// Commit in request order and put mutation accounting ahead of potentially truncated file views.
fn execute(repo: &Repository, patch: &str, control: &Control) -> String {
    let operations = match engine::parse(patch) {
        Ok(operations) => operations,
        Err(error) => return format!("Not applied; no target files changed. {error}"),
    };
    let labels: Vec<String> = operations.iter().map(operation_label).collect();
    let mut applied = Vec::new();
    let mut warnings = Vec::new();
    let mut views = String::new();
    for (index, operation) in operations.into_iter().enumerate() {
        let result = prepare(repo, operation, control)
            .and_then(|change| stage(change, control))
            .and_then(|ready| commit(ready, control, &mut applied, &mut warnings, &mut views));
        match result {
            Ok(true) => {}
            Ok(false) => {
                return stopped(
                    "commit completed with a filesystem error; subsequent operations not attempted",
                    &applied,
                    &labels[index + 1..],
                    &warnings,
                    &views,
                );
            }
            Err(error) => return stopped(&error, &applied, &labels[index..], &warnings, &views),
        }
    }
    format!(
        "Patch applied.\n{}{}{}",
        list(&applied),
        warning_text(&warnings),
        views
    )
}

/// Describe unattempted operations from the request without resolving paths before their turn.
fn operation_label(operation: &Operation) -> String {
    match operation {
        Operation::Add { path, .. } => format!("add {path}"),
        Operation::Delete { path } => format!("delete {path}"),
        Operation::Update {
            path,
            move_to: Some(destination),
            ..
        } => format!("move {path} -> {destination}"),
        Operation::Update { path, .. } => format!("update {path}"),
        Operation::Replace { path, .. } => format!("replace {path}"),
    }
}

/// Commit one operation. False means it committed but a durability failure requires stopping.
fn commit(
    mut item: Ready,
    control: &Control,
    applied: &mut Vec<String>,
    warnings: &mut Vec<String>,
    views: &mut String,
) -> Result<bool, String> {
    if let Err(error) = control.check() {
        warnings.extend(cleanup(std::slice::from_mut(&mut item)));
        return Err(error);
    }
    let Ready {
        label,
        view,
        action,
    } = item;
    let result = match action {
        Action::Write(staged) => staged.commit().map_err(|e| e.to_string()),
        Action::Delete(file) => file.remove().map_err(|e| e.to_string()),
        Action::Rename(source, destination) => {
            source.rename_to(destination).map_err(|e| e.to_string())
        }
        Action::Move(staged, source, destination, edited_path) => {
            match staged.commit() {
                Err(error) => Err(error.to_string()),
                Ok(receipt) => {
                    // Text editing and rename are distinct filesystem operations.
                    // If rename fails, the source edit has already happened.
                    applied.push(format!("updated {edited_path}; rename pending"));
                    if receipt.warnings.is_empty() {
                        control
                            .check()
                            .and_then(|()| source.rename_to(destination).map_err(|e| e.to_string()))
                            .inspect(|_| {
                                applied.pop();
                            })
                    } else {
                        warnings.extend(receipt.warnings);
                        Err("file update committed but synchronization failed; rename not attempted".into())
                    }
                }
            }
        }
    };
    match result {
        Ok(receipt) => {
            applied.push(label);
            warnings.extend(receipt.warnings);
            views.push_str(&view);
            Ok(warnings.is_empty())
        }
        Err(error) => Err(format!("{label}: {error}")),
    }
}

/// Report actual commits and remaining operations without implying transaction rollback.
fn stopped(
    error: &str,
    applied: &[String],
    pending: &[String],
    warnings: &[String],
    views: &str,
) -> String {
    format!(
        "Patch stopped: {error}\nCommitted:\n{}\nNot completed:\n{}{}{}",
        list(applied),
        list(pending),
        warning_text(warnings),
        views
    )
}

/// Keep empty and nonempty accounting sections explicit rather than implying success by omission.
fn list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".into()
    } else {
        items
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Durability and cleanup failures remain visible even after successful namespace changes.
fn warning_text(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!("\nWarnings:\n{}", list(warnings))
    }
}

/// Show changed regions and two neighboring lines with fresh coordinates, merging overlaps.
fn numbered(path: &str, contents: &str, regions: &[(usize, usize)]) -> String {
    if regions.is_empty() {
        return String::new();
    }
    let lines = engine::source_lines(contents);
    if lines.is_empty() {
        return format!("\n\n{path}: (empty file)");
    }
    let mut view = format!("\n\n{path}:\n");
    let mut shown = 0;
    for &(start, end) in regions {
        let first = start.saturating_sub(3).max(shown);
        let last = end.saturating_add(2).min(lines.len());
        if first >= last {
            continue;
        }
        if shown != 0 && first > shown {
            view.push_str("…\n");
        }
        for (i, line) in lines.iter().enumerate().take(last).skip(first) {
            view.push_str(&format!("{}\t{}\n", i + 1, engine::line_text(line)));
        }
        shown = last;
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        /// Isolate filesystem mutations from the repository and live harness state.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "harness-patch-controller-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        /// Give each case its own directory capability and writable permission signal.
        fn context(&self) -> (Repository, Control) {
            (
                Repository::open(&self.0, Root::Repository).unwrap(),
                Control {
                    canceled: Arc::new(AtomicBool::new(false)),
                    writable: Arc::new(AtomicBool::new(true)),
                },
            )
        }
    }

    impl Drop for Fixture {
        /// Test artifacts never become persistent harness state.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// Malformed syntax anywhere in the request prevents even its first file operation.
    #[test]
    fn parse_failure_precedes_filesystem_changes() {
        let dir = Fixture::new();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Add File: a\n+created\n*** Invalid File: b\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Not applied"), "{result}");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    /// Later operations observe creation, rename, and deletion through the actual filesystem.
    #[test]
    fn dependent_operations_use_real_filesystem_state() {
        let dir = Fixture::new();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Add File: a\n+first\n*** Update File: a\n@@\n-first\n+second\n*** Update File: a\n*** Move to: b\n*** Update File: b\n@@\n-second\n+third\n*** Delete File: b\n*** Add File: a\n+final\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(std::fs::read_to_string(dir.0.join("a")).unwrap(), "final\n");
        assert!(!dir.0.join("b").exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    /// Existing-destination checks and missing-source errors use state at the operation's turn.
    #[test]
    fn namespace_errors_stop_without_overwriting_or_recreating_files() {
        for (first, second, expected_name, expected_contents) in [
            (
                "*** Add File: a\n+first",
                "*** Add File: a\n+second",
                "a",
                "first\n",
            ),
            (
                "*** Update File: a\n*** Move to: b",
                "*** Update File: c\n*** Move to: b",
                "b",
                "A\n",
            ),
            (
                "*** Update File: a\n*** Move to: b",
                "*** Update File: a\n@@\n-A\n+changed",
                "b",
                "A\n",
            ),
        ] {
            let dir = Fixture::new();
            if first.starts_with("*** Update") {
                std::fs::write(dir.0.join("a"), "A\n").unwrap();
                std::fs::write(dir.0.join("c"), "C\n").unwrap();
            }
            let (repo, control) = dir.context();
            let result = execute(
                &repo,
                &format!("*** Begin Patch\n{first}\n{second}\n*** End Patch"),
                &control,
            );
            assert!(result.starts_with("Patch stopped"), "{result}");
            assert_eq!(
                std::fs::read_to_string(dir.0.join(expected_name)).unwrap(),
                expected_contents
            );
            if first.starts_with("*** Update") {
                assert!(!dir.0.join("a").exists());
            }
        }
    }

    /// A later invalid hunk stops the patch while preserving and reporting earlier commits.
    #[test]
    fn later_validation_failure_retains_prior_commits() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("a"), "old\n").unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Add File: new\n+created\n*** Update File: a\n@@\n-missing\n+new\n*** End Patch",
            &control,
        );
        assert!(result.contains("Committed:\n- added new"), "{result}");
        assert!(result.contains("Not completed:\n- update a"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("new")).unwrap(),
            "created\n"
        );
        assert_eq!(std::fs::read_to_string(dir.0.join("a")).unwrap(), "old\n");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    /// Mixed operations retain literal content and return numbered post-edit views.
    #[test]
    fn creates_updates_moves_and_deletes() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("a"), "old\r\nlast").unwrap();
        std::fs::write(dir.0.join("gone"), "unused").unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Add File: new\n+\\n is literal\n*** Update File: a\n*** Move to: b\n@@\n-old\n+changed\n*** Delete File: gone\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("new")).unwrap(),
            "\\n is literal\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("b")).unwrap(),
            "changed\r\nlast"
        );
        assert!(!dir.0.join("a").exists());
        assert!(!dir.0.join("gone").exists());
        assert!(result.contains("1\tchanged"));
    }

    /// Separate hard-linked entries are not rejected because they initially share an inode.
    #[test]
    fn permits_updates_to_hard_linked_paths() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("a"), "old\n").unwrap();
        std::fs::hard_link(dir.0.join("a"), dir.0.join("alias")).unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Update File: a\n@@\n-old\n+A\n*** Update File: alias\n@@\n-old\n+B\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(std::fs::read_to_string(dir.0.join("a")).unwrap(), "A\n");
        assert_eq!(std::fs::read_to_string(dir.0.join("alias")).unwrap(), "B\n");
    }

    /// Revoke permission or cancel between preparation and commit without changing targets.
    #[test]
    fn cancellation_and_permission_revocation_before_commit() {
        for canceled in [true, false] {
            let dir = Fixture::new();
            let (repo, control) = dir.context();
            let prepared = prepare(
                &repo,
                Operation::Add {
                    path: "a".into(),
                    contents: "new\n".into(),
                },
                &control,
            )
            .unwrap();
            let ready = stage(prepared, &control).unwrap();
            if canceled {
                control.canceled.store(true, Ordering::SeqCst);
            } else {
                control.writable.store(false, Ordering::SeqCst);
            }
            let mut applied = Vec::new();
            let result = commit(
                ready,
                &control,
                &mut applied,
                &mut Vec::new(),
                &mut String::new(),
            );
            assert!(result.is_err());
            assert!(applied.is_empty());
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
        }
    }

    /// An OS commit error stops the batch and reports both completed and unattempted operations.
    #[test]
    fn reports_partial_commit_after_filesystem_error() {
        let dir = Fixture::new();
        let (repo, control) = dir.context();
        std::fs::create_dir(dir.0.join("b")).unwrap();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Add File: a\n+A\n*** Add File: b\n+B\n*** Add File: c\n+C\n*** End Patch",
            &control,
        );
        assert!(result.contains("Committed:\n- added a"), "{result}");
        assert!(
            result.contains("Not completed:\n- add b\n- add c"),
            "{result}"
        );
        assert_eq!(std::fs::read_to_string(dir.0.join("a")).unwrap(), "A\n");
        assert!(dir.0.join("b").is_dir());
        assert!(!dir.0.join("c").exists());
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    /// Repeated sections execute in submitted order without rejecting or merging their hunks.
    #[test]
    fn successive_updates_use_current_contents() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("a"), "first\nsecond\n").unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Update File: a\n@@\n-first\n+FIRST\n*** Update File: a\n@@\n-second\n+SECOND\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("a")).unwrap(),
            "FIRST\nSECOND\n"
        );
    }

    /// A pure rename operates on entries without imposing text-read requirements.
    #[test]
    fn native_rename_does_not_decode_binary_files() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("a"), [0xff, 0xfe]).unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Update File: a\n*** Move to: b\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(std::fs::read(dir.0.join("b")).unwrap(), [0xff, 0xfe]);
        assert!(!dir.0.join("a").exists());
    }

    /// Update follows the link; native rename moves the named link without repairing its target.
    #[test]
    fn edit_then_rename_uses_filesystem_symlink_semantics() {
        use std::os::unix::fs::symlink;
        let dir = Fixture::new();
        std::fs::create_dir(dir.0.join("nested")).unwrap();
        std::fs::write(dir.0.join("target"), "old\n").unwrap();
        symlink("target", dir.0.join("link")).unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Update File: link\n*** Move to: nested/link\n@@\n-old\n+new\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert_eq!(
            std::fs::read_to_string(dir.0.join("target")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_link(dir.0.join("nested/link")).unwrap(),
            Path::new("target")
        );
        assert!(!dir.0.join("nested/link").exists());
        assert!(std::fs::symlink_metadata(dir.0.join("link")).is_err());
        assert!(result.contains("updated target; moved link -> nested/link"));
        assert!(result.contains("\n\ntarget:\n1\tnew"));
        assert!(!result.contains("\n\nnested/link:\n"));
    }

    /// Replace edits through a relative link like Update, keeping the link, mode, and CRLF endings.
    #[test]
    #[cfg(unix)]
    fn replace_follows_links_and_keeps_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = Fixture::new();
        std::fs::write(dir.0.join("target"), "old\r\nlines\r\n").unwrap();
        std::fs::set_permissions(dir.0.join("target"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        symlink("target", dir.0.join("link")).unwrap();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Replace File: link\n+fresh\n+text\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch applied"), "{result}");
        assert!(result.contains("- replaced link"), "{result}");
        assert!(
            result.contains("\n\nlink:\n1\tfresh\n2\ttext\n"),
            "{result}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.0.join("target")).unwrap(),
            "fresh\r\ntext\r\n"
        );
        assert!(
            std::fs::symlink_metadata(dir.0.join("link"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(dir.0.join("target"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    /// Replace never creates files: a missing target stops the patch before any commit.
    #[test]
    fn replace_requires_existing_file() {
        let dir = Fixture::new();
        let (repo, control) = dir.context();
        let result = execute(
            &repo,
            "*** Begin Patch\n*** Replace File: missing\n+text\n*** Add File: after\n+a\n*** End Patch",
            &control,
        );
        assert!(result.starts_with("Patch stopped: missing:"), "{result}");
        assert!(
            result.contains("Not completed:\n- replace missing\n- add after"),
            "{result}"
        );
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    /// Completion and cancellation both join the owned writer before returning a result.
    #[tokio::test]
    async fn runtime_joins_and_denies_read_only() {
        let dir = Fixture::new();
        let runtime = Runtime::new(&dir.0, &dir.0, false).unwrap();
        let patch = "*** Begin Patch\n*** Add File: a\n+A\n*** End Patch";
        assert!(runtime.start(patch.into(), Access::Repository).is_err());
        runtime.set_permission(true);
        let mut job = runtime.start(patch.into(), Access::Repository).unwrap();
        assert!(job.result().await.starts_with("Patch applied"));
        assert!(job.worker.is_none());
        let mut job = runtime
            .start(
                "*** Begin Patch\n*** Add File: b\n+B\n*** End Patch".into(),
                Access::Repository,
            )
            .unwrap();
        let result = job.stop();
        assert!(job.worker.is_none());
        assert_eq!(
            dir.0.join("b").exists(),
            result.starts_with("Patch applied")
        );
    }
}
