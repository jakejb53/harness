//! Direct text reads through a repository capability, independent of the interpreter.

use cap_std::fs::Dir;
use serde::Deserialize;
use std::io::{self, BufRead, BufReader};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::task::JoinHandle;

#[derive(Deserialize)]
pub struct Arguments {
    pub path: String,
    pub start_line: Option<NonZeroUsize>,
    pub end_line: Option<NonZeroUsize>,
}

pub struct Runtime {
    root: Arc<Dir>,
    readable: Arc<AtomicBool>,
}

impl Runtime {
    /// Establish the launch-directory boundary once; all model paths are resolved inside it.
    pub fn new(root: &Path, readable: bool) -> io::Result<Self> {
        Ok(Self {
            root: Arc::new(Dir::open_ambient_dir(root, cap_std::ambient_authority())?),
            readable: Arc::new(AtomicBool::new(readable)),
        })
    }

    /// Publish permission changes to active reads as well as future calls.
    pub fn set_permission(&self, readable: bool) {
        self.readable.store(readable, Ordering::SeqCst);
    }

    /// Read off the event loop; stopping a call prevents its result from entering the session.
    pub fn start(&self, args: Arguments) -> Job {
        let root = self.root.clone();
        let readable = self.readable.clone();
        let canceled = Arc::new(AtomicBool::new(false));
        let signal = canceled.clone();
        let task = tokio::task::spawn_blocking(move || read(&root, &args, &readable, &signal));
        Job { task, canceled }
    }
}

pub struct Job {
    task: JoinHandle<io::Result<String>>,
    canceled: Arc<AtomicBool>,
}

impl Job {
    /// Return normal filesystem errors as tool text, without exposing host directory names.
    pub async fn result(&mut self) -> String {
        match (&mut self.task).await {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => format!("read_file: {error}"),
            Err(error) => format!("read_file worker failed: {error}"),
        }
    }
}

impl Drop for Job {
    /// Blocking reads stop cooperatively between lines; abandoned results are never consumed.
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// Check authority before opening and before each subsequent line read.
fn allowed(readable: &AtomicBool, canceled: &AtomicBool) -> io::Result<()> {
    if canceled.load(Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "stopped by user",
        ));
    }
    if !readable.load(Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission level is none",
        ));
    }
    Ok(())
}

/// Select an inclusive range while retaining source line numbers; EOF naturally ends the range.
fn read(
    root: &Dir,
    args: &Arguments,
    readable: &AtomicBool,
    canceled: &AtomicBool,
) -> io::Result<String> {
    allowed(readable, canceled)?;
    let file = root
        .open(&args.path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", args.path)))?;
    let mut input = BufReader::new(file);
    let start = args.start_line.map_or(1, NonZeroUsize::get);
    let mut number = 1;
    let mut line = String::new();
    let mut output = String::new();
    while args.end_line.is_none_or(|end| number <= end.get()) {
        allowed(readable, canceled)?;
        line.clear();
        if input.read_line(&mut line).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{} at line {number}: {error}", args.path),
            )
        })? == 0
        {
            break;
        }
        if number >= start {
            let text = line.strip_suffix('\n').map_or(line.as_str(), |text| {
                text.strip_suffix('\r').unwrap_or(text)
            });
            output.push_str(&format!("{number}\t{text}\n"));
        }
        number += 1;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);

    impl Fixture {
        /// Keep all test data in a disposable directory, outside live harness state.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "harness-read-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        /// Remove only the directory this fixture created.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    /// Optional bounds retain the file's coordinates and stop normally at EOF.
    #[tokio::test]
    async fn ranges_and_literal_contents() {
        let dir = Fixture::new();
        std::fs::write(dir.0.join("f"), "one\r\n\\two\n三").unwrap();
        let runtime = Runtime::new(&dir.0, true).unwrap();
        for (start, end, expected) in [
            (None, None, "1\tone\n2\t\\two\n3\t三\n"),
            (Some(2), Some(2), "2\t\\two\n"),
            (Some(2), None, "2\t\\two\n3\t三\n"),
            (None, Some(1), "1\tone\n"),
            (Some(3), Some(99), "3\t三\n"),
            (Some(99), None, ""),
        ] {
            let mut job = runtime.start(Arguments {
                path: "f".into(),
                start_line: start.and_then(NonZeroUsize::new),
                end_line: end.and_then(NonZeroUsize::new),
            });
            assert_eq!(job.result().await, expected);
        }
    }

    /// Reads use the same permission and containment boundary for literal paths and symlinks.
    #[tokio::test]
    async fn permissions_paths_and_errors() {
        use std::os::unix::fs::symlink;
        let dir = Fixture::new();
        std::fs::write(dir.0.join("f"), "content").unwrap();
        symlink("f", dir.0.join("link")).unwrap();
        symlink(dir.0.join("f"), dir.0.join("absolute-link")).unwrap();
        symlink("../outside", dir.0.join("escape")).unwrap();
        let runtime = Runtime::new(&dir.0, false).unwrap();
        let args = |path: String| Arguments {
            path,
            start_line: None,
            end_line: None,
        };
        assert!(
            runtime
                .start(args("f".into()))
                .result()
                .await
                .contains("permission level is none")
        );
        runtime.set_permission(true);
        assert_eq!(
            runtime.start(args("link".into())).result().await,
            "1\tcontent\n"
        );
        for path in [
            dir.0.join("f").display().to_string(),
            "../outside".into(),
            "absolute-link".into(),
            "escape".into(),
            "missing".into(),
        ] {
            assert!(
                runtime
                    .start(args(path))
                    .result()
                    .await
                    .starts_with("read_file:")
            );
        }
        std::fs::write(dir.0.join("invalid"), [0xff]).unwrap();
        assert!(
            runtime
                .start(args("invalid".into()))
                .result()
                .await
                .starts_with("read_file:")
        );
        std::fs::write(dir.0.join("empty"), "").unwrap();
        assert_eq!(runtime.start(args("empty".into())).result().await, "");
    }

    /// Cancellation and revocation stop further reads, without requiring filesystem mutations.
    #[test]
    fn stops_before_reading() {
        let dir = Fixture::new();
        let root = Dir::open_ambient_dir(&dir.0, cap_std::ambient_authority()).unwrap();
        let args = Arguments {
            path: "missing".into(),
            start_line: None,
            end_line: None,
        };
        assert_eq!(
            read(&root, &args, &AtomicBool::new(true), &AtomicBool::new(true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            read(
                &root,
                &args,
                &AtomicBool::new(false),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
