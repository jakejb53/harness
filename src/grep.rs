//! Structured text searches backed by ripgrep and confined to the root a call names.

use ::grep::printer::StandardBuilder;
use ::grep::regex::{RegexMatcher, RegexMatcherBuilder};
use ::grep::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::task::JoinHandle;

use crate::root::{Access, Root};

pub const DESCRIPTION: &str = "Search repository text files. Pattern is an extended regex unless fixed is true. Path is a literal repository-relative file or directory and defaults to the repository root; directories are searched recursively, including hidden and ignored files. Include is a case-sensitive glob matched against file basenames at every depth; empty means no filter. Matching and context lines are always file:line:text, with -- between context groups. Binary files are skipped. Recursive symlinks and special files are skipped with brief warnings. Reads require read-only or read-write permission and cannot escape the repository or follow absolute symlink targets. Generated filenames and diagnostics are repository-relative. The existing output limit applies.";

#[derive(Deserialize)]
pub struct Arguments {
    pub pattern: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub fixed: bool,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub context: usize,
    #[serde(default)]
    pub include: String,
}

/// An omitted path explicitly means the top of the root the call names, independently of
/// the host working directory.
fn default_path() -> String {
    ".".into()
}

pub struct Runtime {
    /// The launch directory; searches follow `readable`.
    repository: Arc<Dir>,
    /// The store, readable at every permission level.
    store: Arc<Dir>,
    /// Whether the permission level allows repository reads.
    readable: Arc<AtomicBool>,
}

impl Runtime {
    /// Acquire ambient authority only once, when establishing the repository and store
    /// boundaries.
    pub fn new(repository: &Path, store: &Path, readable: bool) -> io::Result<Self> {
        Ok(Self {
            repository: Arc::new(Dir::open_ambient_dir(
                repository,
                cap_std::ambient_authority(),
            )?),
            store: Arc::new(Dir::open_ambient_dir(store, cap_std::ambient_authority())?),
            readable: Arc::new(AtomicBool::new(readable)),
        })
    }

    /// Permission changes affect the next filesystem operation of an active repository
    /// search too.
    pub fn set_permission(&self, readable: bool) {
        self.readable.store(readable, Ordering::SeqCst);
    }

    /// Keep pattern compilation, traversal, and searching off the UI thread, under one
    /// cancellable job confined to the root `access` names.
    pub fn start(&self, args: Arguments, access: Access) -> Job {
        let name = access.root();
        let (root, readable) = access.readable(&self.repository, &self.store, &self.readable);
        let canceled = Arc::new(AtomicBool::new(false));
        let signal = canceled.clone();
        let task =
            tokio::task::spawn_blocking(move || search(&root, name, &args, &readable, &signal));
        Job { task, canceled }
    }
}

pub struct Job {
    task: JoinHandle<io::Result<String>>,
    canceled: Arc<AtomicBool>,
}

impl Job {
    /// Normal failures become tool results and never expose the ambient path of the root.
    pub async fn result(&mut self) -> String {
        match (&mut self.task).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => format!("grep: {error}"),
            Err(error) => format!("grep worker failed: {error}"),
        }
    }
}

impl Drop for Job {
    /// A stopped turn discards its result; the worker cooperates at read and output boundaries.
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// Check user authority at each traversal, read, and output boundary.
fn allowed(readable: &AtomicBool, canceled: &AtomicBool) -> io::Result<()> {
    if canceled.load(Ordering::SeqCst) {
        // Read/write helpers retry Interrupted; cancellation must terminate those loops.
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

struct CheckedReader<'a, R> {
    inner: R,
    readable: &'a AtomicBool,
    canceled: &'a AtomicBool,
    binary: &'a mut bool,
}

impl<R: Read> Read for CheckedReader<'_, R> {
    /// Track NUL detection while reading so all earlier output for a binary file is discarded too.
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        allowed(self.readable, self.canceled)?;
        let count = self.inner.read(bytes)?;
        *self.binary |= bytes[..count].contains(&0);
        Ok(count)
    }
}

struct CheckedOutput<'a> {
    bytes: Vec<u8>,
    readable: &'a AtomicBool,
    canceled: &'a AtomicBool,
}

impl Write for CheckedOutput<'_> {
    /// Buffered matches still honor permission revocation and cancellation before being returned.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        allowed(self.readable, self.canceled)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// The underlying output is in memory and needs no flushing.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Search<'a> {
    root: &'a Dir,
    args: &'a Arguments,
    readable: &'a AtomicBool,
    canceled: &'a AtomicBool,
    matcher: RegexMatcher,
    include: Option<GlobMatcher>,
    searcher: Searcher,
    output: Vec<u8>,
    diagnostics: Vec<String>,
}

/// Compile the pattern once and retain normal grep output plus visible per-file diagnostics.
/// `root` is the capability of the root `name`, which errors about the call's path name
/// ("store-relative", "repository-relative").
fn search(
    root: &Dir,
    name: Root,
    args: &Arguments,
    readable: &AtomicBool,
    canceled: &AtomicBool,
) -> io::Result<String> {
    allowed(readable, canceled)?;
    let path = Path::new(&args.path);
    // Reject absolute operands without echoing host directory names into diagnostics.
    if path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path must be {}-relative", name.as_str()),
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
    let patterns: Vec<_> = if args.pattern.is_empty() {
        vec![""]
    } else {
        args.pattern.split_terminator('\n').collect()
    };
    let matcher = RegexMatcherBuilder::new()
        .multi_line(true)
        .line_terminator(Some(b'\n'))
        .fixed_strings(args.fixed)
        .case_insensitive(args.ignore_case)
        .build_many(&patterns)
        .map_err(io::Error::other)?;
    let searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(args.context)
        .after_context(args.context)
        .bom_sniffing(false)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut search = Search {
        root,
        args,
        readable,
        canceled,
        matcher,
        include,
        searcher,
        output: Vec::new(),
        diagnostics: Vec::new(),
    };
    search.visit(path, true)?;
    allowed(readable, canceled)?;
    // Diagnostics precede matches so an output cap cannot disguise an incomplete search.
    let mut result = search.diagnostics.join("\n");
    if !result.is_empty() && !search.output.is_empty() {
        result.push('\n');
    }
    // Match host-command output: tool messages encode arbitrary output bytes with UTF-8 replacement.
    result.push_str(&String::from_utf8_lossy(&search.output));
    Ok(result)
}

impl Search<'_> {
    /// Include skipped paths in the saved tool result so omitted inputs are visible to the caller.
    fn warn_skipped(&mut self, path: &Path, kind: &str) {
        self.diagnostics
            .push(format!("grep: warning: {}: skipped {kind}", path.display()));
    }

    /// File failures remain visible while traversal continues; revoked authority stops the call.
    fn visit(&mut self, path: &Path, operand: bool) -> io::Result<()> {
        allowed(self.readable, self.canceled)?;
        if let Err(error) = self.search_path(path, operand) {
            allowed(self.readable, self.canceled)?;
            self.diagnostics
                .push(format!("grep: {}: {error}", path.display()));
        }
        Ok(())
    }

    /// Every lookup uses the capability for the root the call names, including paths reached
    /// recursively.
    fn search_path(&mut self, path: &Path, operand: bool) -> io::Result<()> {
        let metadata = self.root.symlink_metadata(path)?;
        // Traversal never follows links; direct file operands retain confined link-following.
        if !operand && metadata.file_type().is_symlink() {
            self.warn_skipped(path, "symlink");
            return Ok(());
        }
        let metadata = if metadata.file_type().is_symlink() {
            allowed(self.readable, self.canceled)?;
            let target = self.root.metadata(path)?;
            if target.is_dir() {
                self.warn_skipped(path, "symlink");
                return Ok(());
            }
            target
        } else {
            metadata
        };
        if metadata.is_dir() {
            allowed(self.readable, self.canceled)?;
            for entry in self.root.read_dir(path)? {
                allowed(self.readable, self.canceled)?;
                let child = path.join(entry?.file_name());
                self.visit(&child, false)?;
            }
            return Ok(());
        }
        if !metadata.is_file() {
            self.warn_skipped(path, "non-regular file");
            return Ok(());
        }
        // Filter only file basenames; a nonmatching directory can contain matching descendants.
        if let Some(include) = &self.include
            && !path
                .file_name()
                .is_some_and(|name| include.is_match(Path::new(name)))
        {
            return Ok(());
        }
        allowed(self.readable, self.canceled)?;
        // Metadata can race with replacement by a FIFO. Open without blocking, then check the handle.
        let file = self.root.open_with(
            path,
            OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK),
        )?;
        allowed(self.readable, self.canceled)?;
        if !file.metadata()?.is_file() {
            self.warn_skipped(path, "non-regular file");
            return Ok(());
        }
        let mut binary = false;
        let reader = CheckedReader {
            inner: file,
            readable: self.readable,
            canceled: self.canceled,
            binary: &mut binary,
        };
        let output = CheckedOutput {
            bytes: Vec::new(),
            readable: self.readable,
            canceled: self.canceled,
        };
        let mut printer = StandardBuilder::new()
            .path(true)
            .heading(false)
            .separator_field_context(b":".to_vec())
            .build_no_color(output);
        let result = self.searcher.search_reader(
            &self.matcher,
            reader,
            printer.sink_with_path(&self.matcher, path),
        );
        let bytes = printer.into_inner().into_inner().bytes;
        // Discard ignored file output without losing any read error.
        if bytes.is_empty() || binary {
            return result;
        }
        if !self.output.is_empty() && self.args.context > 0 {
            self.output.extend_from_slice(b"--\n");
        }
        self.output.extend_from_slice(&bytes);
        result
    }
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
                std::env::temp_dir().join(format!("harness-grep-{}-{serial}", std::process::id()));
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

        /// Exercise argument defaults, validation, and capability-backed search through one boundary.
        fn run_result(&self, arguments: Value) -> io::Result<String> {
            let args = serde_json::from_value(arguments).map_err(io::Error::other)?;
            search(
                &self.root,
                Root::Repository,
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

    /// Optional parameters have the advertised defaults; the pattern remains required.
    #[test]
    fn grep_arguments_and_defaults() {
        let args: Arguments = serde_json::from_value(json!({"pattern": "this|that"})).unwrap();
        assert_eq!(args.path, ".");
        assert!(!args.fixed && !args.ignore_case);
        assert_eq!(args.context, 0);
        assert_eq!(args.include, "");
        for value in [
            json!({}),
            json!({"command": "grep x f"}),
            json!({"pattern": 3}),
            json!({"pattern": "x", "context": -1}),
            json!({"pattern": "x", "fixed": "true"}),
        ] {
            assert!(serde_json::from_value::<Arguments>(value).is_err());
        }
    }

    /// Matching and context lines always use the same file:line:text format, merging overlapping groups.
    #[test]
    fn grep_regex_context_and_filenames() {
        let repo = Fixture::new();
        repo.write(
            "f",
            "first\nthis\nbetween\nthat\nfifth\nsixth\nseventh\nthis|that\n",
        );
        assert_eq!(
            repo.run(json!({"pattern":"^(this|that)$","path":"f"})),
            "f:2:this\nf:4:that\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"^this","path":"f","context":1})),
            "f:1:first\nf:2:this\nf:3:between\n--\nf:7:seventh\nf:8:this|that\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"^(this|that)$","path":"f","context":1})),
            "f:1:first\nf:2:this\nf:3:between\nf:4:that\nf:5:fifth\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"^that$","path":"f","context":0})),
            "f:4:that\n"
        );
        assert_eq!(repo.run(json!({"pattern":"absent","path":"f"})), "");
        repo.write("g", "this\n");
        let output = repo.run(json!({"pattern":"^(this|that)$","context":1}));
        assert!(output.contains("--\n"));
        assert!(output.contains("./f:2:this\n"));
        assert!(output.contains("./g:1:this\n"));
    }

    /// Fixed strings and case folding compose without interpreting quoting or path metacharacters.
    #[test]
    fn grep_fixed_strings_and_literal_paths() {
        let repo = Fixture::new();
        repo.write("file with spaces", "this\nTHIS|THAT\nother.*\n");
        assert_eq!(
            repo.run(json!({"pattern":"this|that","path":"file with spaces"})),
            "file with spaces:1:this\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"this|that","path":"file with spaces","fixed":true})),
            ""
        );
        assert_eq!(
            repo.run(json!({"pattern":"this|that","path":"file with spaces","fixed":true,"ignore_case":true})),
            "file with spaces:2:THIS|THAT\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"this|that","path":"file with spaces","ignore_case":true})),
            "file with spaces:1:this\nfile with spaces:2:THIS|THAT\n"
        );
        assert_eq!(
            repo.run(
                json!({"pattern":"THIS|THAT\nother.*","path":"file with spaces","fixed":true})
            ),
            "file with spaces:2:THIS|THAT\nfile with spaces:3:other.*\n"
        );
        repo.write("-file", "-pattern\n");
        assert_eq!(
            repo.run(json!({"pattern":"-pattern","path":"-file","fixed":true})),
            "-file:1:-pattern\n"
        );
        repo.write("$(not-a-command)", "found\n");
        assert_eq!(
            repo.run(json!({"pattern":"found","path":"$(not-a-command)"})),
            "$(not-a-command):1:found\n"
        );
        repo.write("-", "dash\n");
        assert_eq!(repo.run(json!({"pattern":"dash","path":"-"})), "-:1:dash\n");
    }

    /// Omitted paths search the repository recursively, include hidden files, and always skip binaries.
    #[test]
    fn grep_recursive_search_and_binary_files() {
        let repo = Fixture::new();
        for path in ["src/a", "src/sub/b", "src/.hidden", "src/ignored"] {
            repo.write(path, "needle\n");
        }
        repo.write("src/.gitignore", "ignored\n");
        repo.write("src/binary", b"needle\0tail\n");
        let output = repo.run(json!({"pattern":"needle"}));
        let mut lines: Vec<_> = output.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "./src/.hidden:1:needle",
                "./src/a:1:needle",
                "./src/ignored:1:needle",
                "./src/sub/b:1:needle"
            ]
        );
        assert!(!output.contains(repo.path.to_str().unwrap()));
        assert_eq!(
            repo.run(json!({"pattern":"needle","path":"src/binary"})),
            ""
        );
        let late_binary = format!("needle\n{}\0needle\n", "unmatched line\n".repeat(10_000));
        repo.write("late-binary", late_binary);
        assert_eq!(
            repo.run(json!({"pattern":"needle","path":"late-binary","context":1})),
            ""
        );
    }

    /// Basename filtering reaches every depth and stays case-sensitive independently of content matching.
    #[test]
    fn grep_include_and_invalid_patterns() {
        let repo = Fixture::new();
        for path in ["root.rs", "dir/sub/deep.rs", "dir/note.txt", "dir/case.RS"] {
            repo.write(path, "Needle\n");
        }
        let output = repo.run(json!({"pattern":"needle","ignore_case":true,"include":"*.rs"}));
        let mut lines: Vec<_> = output.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, ["./dir/sub/deep.rs:1:Needle", "./root.rs:1:Needle"]);
        assert_eq!(
            repo.run(json!({"pattern":"Needle","path":"dir/sub/deep.rs","include":"deep.rs"})),
            "dir/sub/deep.rs:1:Needle\n"
        );
        assert_eq!(
            repo.run(json!({"pattern":"Needle","path":"root.rs","include":"*.txt"})),
            ""
        );
        assert_eq!(
            repo.run(json!({"pattern":"Needle","include":""}))
                .lines()
                .count(),
            4
        );
        assert!(
            repo.run_result(json!({"pattern":"Needle","include":"["}))
                .unwrap_err()
                .to_string()
                .contains("include")
        );
        assert!(repo.run_result(json!({"pattern":"("})).is_err());
    }

    /// Pipes and sockets never wait for a producer, including when explicitly named or linked.
    #[test]
    fn grep_warns_on_special_files() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::UnixListener;

        let repo = Fixture::new();
        repo.write("f", "needle\n");
        let path = CString::new(repo.path.join("pipe").as_os_str().as_bytes()).unwrap();
        // The CString supplies a live, NUL-terminated path entirely inside this scratch fixture.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let _socket = UnixListener::bind(repo.path.join("socket")).unwrap();
        symlink("pipe", repo.path.join("pipe-link")).unwrap();
        for path in ["pipe", "socket", "pipe-link"] {
            assert_eq!(
                repo.run(json!({"pattern":"needle","path":path})),
                format!("grep: warning: {path}: skipped non-regular file")
            );
        }
        let output = repo.run(json!({"pattern":"needle"}));
        let mut lines: Vec<_> = output.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "./f:1:needle",
                "grep: warning: ./pipe-link: skipped symlink",
                "grep: warning: ./pipe: skipped non-regular file",
                "grep: warning: ./socket: skipped non-regular file",
            ]
        );
    }

    /// Capability checks reject direct and symlink escapes while keeping labels and errors relative.
    #[test]
    fn grep_containment_and_diagnostics() {
        let repo = Fixture::new();
        let outside = Fixture::new();
        repo.write("inside", "needle\n");
        outside.write("secret", "needle OUTSIDE_SECRET\n");
        symlink("inside", repo.path.join("link")).unwrap();
        symlink(outside.path.join("secret"), repo.path.join("absolute-link")).unwrap();
        let outside_name = outside.path.file_name().unwrap().to_str().unwrap();
        symlink(
            format!("../{outside_name}/secret"),
            repo.path.join("escape"),
        )
        .unwrap();
        assert_eq!(
            repo.run(json!({"pattern":"needle","path":"link"})),
            "link:1:needle\n"
        );
        for operand in [
            "absolute-link".to_string(),
            "escape".into(),
            format!("../{outside_name}/secret"),
        ] {
            let output = repo.run(json!({"pattern":"needle","path":operand}));
            assert!(output.starts_with("grep:"), "{output}");
            assert!(!output.contains("OUTSIDE_SECRET"));
            assert!(!output.contains(repo.path.to_str().unwrap()));
            assert!(!output.contains(outside.path.to_str().unwrap()));
        }
        let absolute = repo.path.join("inside").to_str().unwrap().to_owned();
        assert!(
            !repo
                .run_result(json!({"pattern":"needle","path":absolute}))
                .unwrap_err()
                .to_string()
                .contains(repo.path.to_str().unwrap())
        );
        let output = repo.run(json!({"pattern":"needle","path":"missing"}));
        assert!(output.starts_with("grep: missing:"));
        symlink(".", repo.path.join("loop")).unwrap();
        let output = repo.run(json!({"pattern":"needle"}));
        let mut lines: Vec<_> = output.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            [
                "./inside:1:needle",
                "grep: warning: ./absolute-link: skipped symlink",
                "grep: warning: ./escape: skipped symlink",
                "grep: warning: ./link: skipped symlink",
                "grep: warning: ./loop: skipped symlink",
            ]
        );
        assert_eq!(
            repo.run(json!({"pattern":"needle","path":"loop"})),
            "grep: warning: loop: skipped symlink"
        );
    }

    /// Active buffered work observes revocation and cancellation at its next read or write.
    #[test]
    fn grep_permission_and_cancellation_boundaries() {
        let readable = AtomicBool::new(true);
        let canceled = AtomicBool::new(false);
        let mut binary = false;
        let mut input = CheckedReader {
            inner: &b"first\nsecond\n"[..],
            readable: &readable,
            canceled: &canceled,
            binary: &mut binary,
        };
        let mut bytes = [0; 6];
        assert_eq!(input.read(&mut bytes).unwrap(), 6);
        readable.store(false, Ordering::SeqCst);
        assert_eq!(
            input.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        readable.store(true, Ordering::SeqCst);
        canceled.store(true, Ordering::SeqCst);
        assert_eq!(
            input.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        let mut output = CheckedOutput {
            bytes: Vec::new(),
            readable: &readable,
            canceled: &canceled,
        };
        assert_eq!(
            output.write_all(b"late result").unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(output.bytes.is_empty());
        let matcher = RegexMatcher::new("needle").unwrap();
        let mut printer = StandardBuilder::new().build_no_color(output);
        let error = Searcher::new()
            .search_slice(&matcher, b"needle\n", printer.sink(&matcher))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "stopped by user");
    }

    /// Runtime permission updates and job abandonment use the same flags the worker checks.
    #[tokio::test]
    async fn grep_runtime_permissions_and_job_drop() {
        let repo = Fixture::new();
        repo.write("f", "needle\n");
        let runtime = Runtime::new(&repo.path, &repo.path, false).unwrap();
        let args = || serde_json::from_value(json!({"pattern":"needle","path":"f"})).unwrap();
        assert!(
            runtime
                .start(args(), Access::Repository)
                .result()
                .await
                .contains("permission level is none")
        );
        runtime.set_permission(true);
        assert_eq!(
            runtime.start(args(), Access::Repository).result().await,
            "f:1:needle\n"
        );
        let job = runtime.start(args(), Access::Repository);
        let signal = job.canceled.clone();
        drop(job);
        assert!(signal.load(Ordering::SeqCst));
    }
}
