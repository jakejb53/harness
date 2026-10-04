//! Directory entry metadata through repository capabilities, without opening file contents.

use cap_std::fs::{Dir, DirEntry, Metadata};
use chrono::{DateTime, SecondsFormat, Utc};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

pub const DESCRIPTION: &str = "List a repository-relative directory; path defaults to the repository root. Non-recursive by default. Include is a case-sensitive basename glob filtering emitted entries at every depth; empty means all entries. Recursive traversal enters nonmatching directories too. Hidden entries are included. Output is sorted by relative path, one row per entry: quoted/escaped path, exact metadata size in bytes, and UTC mtime with nanosecond precision. Directories have a trailing /; symlinks and special files have type labels and use their own metadata. Directory sizes are metadata sizes, not recursive totals. Recursive traversal never follows symlinks. Requires read-only or read-write permission; repository containment and the existing output limit apply.";

#[derive(Deserialize)]
pub struct Arguments {
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub include: String,
}

/// An omitted path explicitly means the launch repository, independently of the host working directory.
fn default_path() -> String {
    ".".into()
}

pub struct Runtime {
    root: Arc<Dir>,
    readable: Arc<AtomicBool>,
}

impl Runtime {
    /// Acquire ambient authority only once, when establishing the launch-directory boundary.
    pub fn new(root: &Path, readable: bool) -> io::Result<Self> {
        Ok(Self {
            root: Arc::new(Dir::open_ambient_dir(root, cap_std::ambient_authority())?),
            readable: Arc::new(AtomicBool::new(readable)),
        })
    }

    /// Permission changes affect the next metadata operation of an active listing too.
    pub fn set_permission(&self, readable: bool) {
        self.readable.store(readable, Ordering::SeqCst);
    }

    /// Keep directory traversal and metadata formatting off the UI thread in one cancellable job.
    pub fn start(&self, args: Arguments) -> Job {
        let root = self.root.clone();
        let readable = self.readable.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        let signal = canceled.clone();
        let task = tokio::task::spawn_blocking(move || listing(&root, &args, &readable, &signal));
        Job { task, canceled }
    }
}

pub struct Job {
    task: JoinHandle<io::Result<String>>,
    canceled: Arc<AtomicBool>,
}

impl Job {
    /// Normal failures become tool results and never expose the ambient repository path.
    pub async fn result(&mut self) -> String {
        match (&mut self.task).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => format!("list: {error}"),
            Err(error) => format!("list worker failed: {error}"),
        }
    }
}

impl Drop for Job {
    /// A stopped turn discards its result; traversal cooperates between metadata operations.
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// Check user authority before metadata operations and before returning collected rows.
fn allowed(readable: &AtomicBool, canceled: &AtomicBool) -> io::Result<()> {
    if canceled.load(Ordering::SeqCst) {
        // Cancellation is terminal, unlike retryable OS interruptions.
        return Err(io::Error::other("stopped by user"));
    }
    if !readable.load(Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission level is none",
        ));
    }
    Ok(())
}

struct Listing<'a> {
    recursive: bool,
    include: Option<GlobMatcher>,
    readable: &'a AtomicBool,
    canceled: &'a AtomicBool,
    rows: Vec<(OsString, String)>,
    diagnostics: Vec<String>,
}

/// Resolve the requested directory inside the repository and return freshly measured, sorted entries.
fn listing(
    root: &Dir,
    args: &Arguments,
    readable: &AtomicBool,
    canceled: &AtomicBool,
) -> io::Result<String> {
    allowed(readable, canceled)?;
    let path = Path::new(&args.path);
    if path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must be a repository-relative directory",
        ));
    }
    let include = if args.include.is_empty() {
        None
    } else {
        Some(
            Glob::new(&args.include)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid include glob: {error}"),
                    )
                })?
                .compile_matcher(),
        )
    };
    let directory = root
        .open_dir(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{path:?}: {error}")))?;
    let mut listing = Listing {
        recursive: args.recursive,
        include,
        readable,
        canceled,
        rows: Vec::new(),
        diagnostics: Vec::new(),
    };
    if let Err(error) = listing.walk(&directory, path) {
        allowed(readable, canceled)?;
        listing.diagnostic(path, &error);
    }
    // Sort underlying paths, not escaped presentation strings or type labels.
    listing.rows.sort_by(|left, right| left.0.cmp(&right.0));
    let mut output = listing.diagnostics.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    for (_, row) in listing.rows {
        allowed(readable, canceled)?;
        output.push_str(&row);
        output.push('\n');
    }
    allowed(readable, canceled)?;
    Ok(output)
}

impl Listing<'_> {
    /// Keep failed entry metadata visible alongside successful rows without exposing ambient paths.
    fn diagnostic(&mut self, path: &Path, error: &io::Error) {
        self.diagnostics.push(format!("list: {path:?}: {error}"));
    }

    /// Enumerate an already-open directory, retaining its authority across recursive descents.
    fn walk(&mut self, directory: &Dir, path: &Path) -> io::Result<()> {
        allowed(self.readable, self.canceled)?;
        for entry in directory.entries()? {
            allowed(self.readable, self.canceled)?;
            let entry = entry?;
            let name = entry.file_name();
            let child_path = path.join(&name);
            if let Err(error) = self.entry(directory, &entry, &name, &child_path) {
                allowed(self.readable, self.canceled)?;
                self.diagnostic(&child_path, &error);
            }
        }
        Ok(())
    }

    /// Read link metadata itself; an include mismatch only suppresses the row, never descent.
    fn entry(
        &mut self,
        directory: &Dir,
        entry: &DirEntry,
        name: &OsStr,
        path: &Path,
    ) -> io::Result<()> {
        allowed(self.readable, self.canceled)?;
        let metadata = entry.metadata()?;
        if self
            .include
            .as_ref()
            .is_none_or(|glob| glob.is_match(Path::new(name)))
        {
            let row = format_entry(path, &metadata)?;
            self.rows.push((path.as_os_str().to_owned(), row));
        }
        if self.recursive && metadata.is_dir() {
            allowed(self.readable, self.canceled)?;
            let child = open_child_directory(directory, name)?;
            self.walk(&child, path)?;
        }
        Ok(())
    }
}

/// Open one enumerated basename without following links, even if it changed after metadata collection.
fn open_child_directory(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    let mut components = Path::new(name).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory entry must be a single basename",
        ));
    }
    let name = CString::new(name.as_bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    // The borrowed parent is a confined directory capability; the validated basename cannot escape it.
    // O_NOFOLLOW and O_DIRECTORY make the final type check atomic with open, without touching contents.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // A successful openat returns a new descriptor; transfer its ownership exactly once into the Dir.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    Ok(Dir::from_std_file(file))
}

/// Preserve exact entry sizes and escape path bytes so whitespace and control characters cannot split rows.
fn format_entry(path: &Path, metadata: &Metadata) -> io::Result<String> {
    let mut label = path.as_os_str().to_owned();
    let kind = if metadata.is_dir() {
        label.push("/");
        ""
    } else if metadata.file_type().is_symlink() {
        " [symlink]"
    } else if metadata.is_file() {
        ""
    } else {
        " [special]"
    };
    let modified = format_mtime(metadata.modified()?.into_std())?;
    Ok(format!("{label:?}{kind}  {}  {modified}", metadata.len()))
}

/// Format UTC mtime without dropping fractional seconds, including dates before the Unix epoch.
fn format_mtime(time: SystemTime) -> io::Result<String> {
    // Duration's u64 seconds fit in i128 nanoseconds; Euclidean division preserves pre-epoch fractions.
    let nanos = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "modification time is outside the supported date range",
        )
    };
    let seconds = i64::try_from(nanos.div_euclid(1_000_000_000)).map_err(|_| invalid())?;
    let subsecond = nanos.rem_euclid(1_000_000_000) as u32;
    let time = DateTime::<Utc>::from_timestamp(seconds, subsecond).ok_or_else(invalid)?;
    Ok(time.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        path: PathBuf,
        root: Dir,
    }

    impl Fixture {
        /// Confine every fixture, including outside-target probes, to disposable scratch directories.
        fn new() -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("harness-list-{}-{serial}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let root = Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
            Self { path, root }
        }

        /// Set up named fixture files without touching the user's repository contents.
        fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
            if let Some(parent) = Path::new(path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                self.root.create_dir_all(parent).unwrap();
            }
            self.root.write(path, contents).unwrap();
        }

        /// Exercise argument defaults, validation, and capability-backed listing through one boundary.
        fn run_result(&self, arguments: Value) -> io::Result<String> {
            let args = serde_json::from_value(arguments).map_err(io::Error::other)?;
            listing(
                &self.root,
                &args,
                &AtomicBool::new(true),
                &AtomicBool::new(false),
            )
        }

        /// Successful fixtures retain the exact text the tool will return before the shared output cap.
        fn run(&self, arguments: Value) -> String {
            self.run_result(arguments).unwrap()
        }
    }

    impl Drop for Fixture {
        /// Remove only the scratch directory allocated by this fixture.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }

    /// Defaults produce top-level entries, exact sizes and nanosecond timestamps, including hidden names.
    #[test]
    fn list_defaults_metadata_and_order() {
        use std::fs::FileTimes;
        use std::time::Duration;

        let repo = Fixture::new();
        repo.write("z", "abc");
        repo.write(".hidden", b"\0binary");
        repo.write("a-dir/nested", "nested contents");
        let modified = UNIX_EPOCH + Duration::new(10, 123_456_789);
        std::fs::File::open(repo.path.join("z"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::File::open(repo.path.join("a-dir"))
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        let args: Arguments = serde_json::from_value(json!({})).unwrap();
        assert_eq!(args.path, ".");
        assert!(!args.recursive);
        assert!(args.include.is_empty());
        let output = repo.run(json!({}));
        let rows: Vec<_> = output.lines().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("\"./.hidden\"  7  "));
        let directory_size = std::fs::metadata(repo.path.join("a-dir")).unwrap().len();
        assert_eq!(
            rows[1],
            format!("\"./a-dir/\"  {directory_size}  1970-01-01T00:00:10.123456789Z")
        );
        assert_eq!(rows[2], "\"./z\"  3  1970-01-01T00:00:10.123456789Z");
        assert!(!output.contains("nested"));
        assert!(!output.contains(repo.path.to_str().unwrap()));
    }

    /// A basename filter controls rows while recursion can still enter nonmatching directories.
    #[test]
    fn list_recursive_include_and_directories() {
        let repo = Fixture::new();
        for path in [
            "root.rs",
            "src/sub/deep.rs",
            "src/note.txt",
            "src/upper.RS",
            "a.rs/inner.txt",
        ] {
            repo.write(path, "x");
        }
        let top = repo.run(json!({"include":"*.rs"}));
        assert_eq!(top.lines().count(), 2);
        assert!(top.lines().next().unwrap().starts_with("\"./a.rs/\"  "));
        assert!(!top.contains("deep.rs"));
        let recursive = repo.run(json!({"recursive":true,"include":"*.rs"}));
        let rows: Vec<_> = recursive.lines().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].starts_with("\"./a.rs/\"  "));
        assert!(rows[1].starts_with("\"./root.rs\"  1  "));
        assert!(rows[2].starts_with("\"./src/sub/deep.rs\"  1  "));
        let sub = repo.run(json!({"path":"src/sub"}));
        assert!(sub.starts_with("\"src/sub/deep.rs\"  1  "));
        assert_eq!(repo.run(json!({"include":"absent"})), "");
        assert!(
            repo.run_result(json!({"include":"["}))
                .unwrap_err()
                .to_string()
                .contains("include")
        );
        assert!(repo.run_result(json!({"path":"root.rs"})).is_err());
    }

    /// Links use their own metadata, special files never open, and recursive links do not duplicate entries.
    #[test]
    fn list_links_and_special_entries() {
        let repo = Fixture::new();
        let outside = Fixture::new();
        repo.write("sub/file", "inside");
        outside.write("private", "OUTSIDE");
        symlink("sub", repo.path.join("link")).unwrap();
        symlink(".", repo.path.join("loop")).unwrap();
        symlink("missing", repo.path.join("broken")).unwrap();
        symlink(&outside.path, repo.path.join("outside")).unwrap();
        let fifo = CString::new(repo.path.join("pipe").as_os_str().as_bytes()).unwrap();
        // This NUL-terminated path names a disposable FIFO inside the fixture, never live state.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let output = repo.run(json!({"recursive":true}));
        assert_eq!(output.lines().count(), 7);
        assert!(output.contains("\"./link\" [symlink]  3  "));
        assert!(output.contains("\"./broken\" [symlink]  7  "));
        assert!(output.contains("\"./pipe\" [special]  "));
        assert!(output.contains("\"./sub/file\"  6  "));
        assert!(!output.contains("link/file"));
        assert!(!output.contains("private"));
        assert!(!output.contains(outside.path.to_str().unwrap()));
        assert!(repo.run_result(json!({"path":"outside"})).is_err());
    }

    /// Directory opens cannot be redirected by a symlink substituted after an entry was inspected.
    #[test]
    fn list_descent_rejects_replaced_links_and_traversal() {
        let repo = Fixture::new();
        repo.root.create_dir("candidate").unwrap();
        repo.root.create_dir("target").unwrap();
        assert!(repo.root.symlink_metadata("candidate").unwrap().is_dir());
        repo.root.remove_dir("candidate").unwrap();
        symlink("target", repo.path.join("candidate")).unwrap();
        assert!(open_child_directory(&repo.root, OsStr::new("candidate")).is_err());
        for invalid in ["..", "target/..", "target/child", "/tmp"] {
            assert!(open_child_directory(&repo.root, OsStr::new(invalid)).is_err());
        }
        let absolute = repo.path.to_str().unwrap();
        let error = repo
            .run_result(json!({"path":absolute}))
            .unwrap_err()
            .to_string();
        assert!(!error.contains(absolute));
        assert!(repo.run_result(json!({"path":".."})).is_err());
    }

    /// Escaping preserves a single output row even for control characters or non-UTF-8 names.
    #[test]
    fn list_escaped_paths_and_pre_epoch_time() {
        use std::os::unix::ffi::OsStringExt;
        use std::time::Duration;

        let repo = Fixture::new();
        repo.write("space name", "x");
        repo.write("line\nname", "x");
        let output = repo.run(json!({}));
        assert_eq!(output.lines().count(), 2);
        assert!(output.contains("\"./space name\"  1  "));
        assert!(output.contains("\"./line\\nname\"  1  "));
        // Some host filesystems reject byte names; exercise their representation independently of creation.
        let byte_name = OsString::from_vec(b"byte-\xff".to_vec());
        let escaped = format_entry(
            Path::new(&byte_name),
            &repo.root.symlink_metadata("space name").unwrap(),
        )
        .unwrap();
        assert!(escaped.contains("\\xFF") || escaped.contains("\\xff"));
        assert!(!escaped.contains('\u{fffd}'));
        assert_eq!(escaped.lines().count(), 1);
        assert_eq!(
            format_mtime(UNIX_EPOCH - Duration::from_nanos(1)).unwrap(),
            "1969-12-31T23:59:59.999999999Z"
        );
    }

    /// Revocation and cancellation prevent even metadata enumeration, including for empty directories.
    #[tokio::test]
    async fn list_permissions_and_cancellation() {
        let repo = Fixture::new();
        repo.write("f", "x");
        let runtime = Runtime::new(&repo.path, false).unwrap();
        let args = || serde_json::from_value(json!({})).unwrap();
        assert!(
            runtime
                .start(args())
                .result()
                .await
                .contains("permission level is none")
        );
        runtime.set_permission(true);
        assert!(
            runtime
                .start(args())
                .result()
                .await
                .starts_with("\"./f\"  1  ")
        );
        let canceled = AtomicBool::new(true);
        assert!(listing(&repo.root, &args(), &AtomicBool::new(true), &canceled).is_err());
        let readable = AtomicBool::new(true);
        let active = AtomicBool::new(false);
        allowed(&readable, &active).unwrap();
        readable.store(false, Ordering::SeqCst);
        assert_eq!(
            allowed(&readable, &active).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let job = runtime.start(args());
        let signal = job.canceled.clone();
        drop(job);
        assert!(signal.load(Ordering::SeqCst));
    }
}
