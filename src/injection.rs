//! User-selected file reads, prepared without running the model's REPL.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::{Map, json};

use crate::session::{FunctionCall, ToolCall};

pub struct PreparedInjection {
    pub call: ToolCall,
    pub content: String,
    pub path: String,
}

/// Only a column-one `@` introduces a path; spaces and quoting remain literal.
pub fn paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix('@').map(str::to_owned))
        .collect()
}

/// Prepare every attachment before the caller commits any conversation changes.
/// These reads have the user's attachment authority, not the model's permissions.
pub fn load(
    paths: Vec<String>,
    root: PathBuf,
    canceled: Arc<AtomicBool>,
) -> Result<Vec<PreparedInjection>> {
    let mut prepared = Vec::with_capacity(paths.len());
    for path in paths {
        let injection = read_file(&path, &root, &canceled)
            .with_context(|| format!("Could not attach {path:?}"))?;
        prepared.push(injection);
    }
    check_canceled(&canceled)?;
    Ok(prepared)
}

/// Read regular files in chunks so cancellation can discard a pending submission.
fn read_file(
    path: &str,
    root: &std::path::Path,
    canceled: &AtomicBool,
) -> Result<PreparedInjection> {
    check_canceled(canceled)?;
    if path.is_empty() {
        bail!("attachment path is empty");
    }
    let absolute = root.join(path);
    if !std::fs::metadata(&absolute)?.is_file() {
        bail!("attachment must be a regular file");
    }
    // Nonblocking open prevents a replacement FIFO from hanging between the
    // path metadata check and opening; check the opened descriptor as well.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&absolute)?;
    if !file.metadata()?.is_file() {
        bail!("attachment must be a regular file");
    }
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        check_canceled(canceled)?;
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    check_canceled(canceled)?;
    let content = String::from_utf8(bytes).context("attachment is not UTF-8 text")?;
    let absolute = absolute.to_str().context("attachment path is not UTF-8")?;
    // JSON string escaping also yields a Python string literal. Reading bytes
    // avoids Python's newline translation, preserving the exact supplied result.
    let literal = serde_json::to_string(absolute)?;
    let code = format!(
        "from pathlib import Path\nprint(Path({literal}).read_bytes().decode('utf-8'), end='')"
    );
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    Ok(PreparedInjection {
        call: ToolCall {
            id: format!(
                "injection-{}-{}-{}",
                chrono::Local::now().timestamp_micros(),
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            kind: "function".into(),
            function: FunctionCall {
                name: "REPL".into(),
                arguments: json!({ "code": code }).to_string(),
                extra: Map::new(),
            },
            extra: Map::new(),
        },
        content,
        path: path.to_owned(),
    })
}

/// Cancellation never returns a partially prepared attachment batch.
fn check_canceled(canceled: &AtomicBool) -> Result<()> {
    if canceled.load(Ordering::Relaxed) {
        bail!("attachment loading canceled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        /// Keep filesystem tests independent and remove their scratch data on drop.
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "harness-injection-test-{}-{}-{}",
                std::process::id(),
                chrono::Local::now().timestamp_nanos_opt().unwrap(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        /// Run the public loader with an uncanceled request.
        fn load(&self, paths: &[&str]) -> Result<Vec<PreparedInjection>> {
            super::load(
                paths.iter().map(|path| (*path).to_owned()).collect(),
                self.0.clone(),
                Arc::new(AtomicBool::new(false)),
            )
        }
    }

    impl Drop for Fixture {
        /// Remove only the unique directory owned by this fixture.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Standalone paths do not acquire shell syntax or whitespace normalization.
    #[test]
    fn literal_line_grammar() {
        assert_eq!(
            paths("hello @inline\n @indented\n@a b \n@\n@\"quoted\"\n@@literal\n@~/x\n"),
            ["a b ", "", "\"quoted\"", "@literal", "~/x"]
        );
    }

    /// Each occurrence has its own identity and preserves contents without limits.
    #[test]
    fn duplicates_and_full_contents() {
        let fixture = Fixture::new();
        let content = format!("{}\r\nlast", "é".repeat(100_000));
        std::fs::write(fixture.0.join("file"), &content).unwrap();
        let loaded = fixture.load(&["file", "file"]).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_ne!(loaded[0].call.id, loaded[1].call.id);
        for injection in loaded {
            assert_eq!(injection.path, "file");
            assert_eq!(injection.content, content);
            assert!(injection.call.id.starts_with("injection-"));
            assert_eq!(injection.call.function.name, "REPL");
        }
    }

    /// Labels stay literal while synthetic code safely quotes the resolved path.
    #[test]
    fn spaces_quotes_unicode_and_absolute_paths() {
        let fixture = Fixture::new();
        let name = "a 'quoted' \"雪\" ";
        let absolute = fixture.0.join(name);
        std::fs::write(&absolute, "hello\r\n").unwrap();
        let loaded = fixture.load(&[name, absolute.to_str().unwrap()]).unwrap();
        assert_eq!(loaded[0].path, name);
        assert_eq!(loaded[1].path, absolute.to_str().unwrap());
        assert_eq!(loaded[0].content, "hello\r\n");
        let arguments: serde_json::Value =
            serde_json::from_str(&loaded[0].call.function.arguments).unwrap();
        let literal = serde_json::to_string(absolute.to_str().unwrap()).unwrap();
        assert_eq!(
            arguments["code"],
            format!(
                "from pathlib import Path\nprint(Path({literal}).read_bytes().decode('utf-8'), end='')"
            )
        );
    }

    /// User attachment authority includes absolute, parent, and symlink paths outside launch root.
    #[test]
    fn reads_outside_launch_root() {
        let fixture = Fixture::new();
        let root = fixture.0.join("repo");
        std::fs::create_dir(&root).unwrap();
        let sibling = fixture.0.join("outside.txt");
        let content = "outside contents\r\n雪";
        std::fs::write(&sibling, content).unwrap();
        std::os::unix::fs::symlink(&sibling, root.join("linked.txt")).unwrap();
        let supplied = vec![
            sibling.to_str().unwrap().to_owned(),
            "../outside.txt".into(),
            "linked.txt".into(),
        ];
        let loaded = load(supplied.clone(), root, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(loaded.len(), 3);
        for (injection, path) in loaded.iter().zip(supplied) {
            assert_eq!(injection.path, path);
            assert_eq!(injection.content, content);
        }
        let ids: std::collections::HashSet<_> =
            loaded.iter().map(|injection| &injection.call.id).collect();
        assert_eq!(ids.len(), 3);
    }

    /// Invalid attachments return errors rather than partial successful batches.
    #[test]
    fn invalid_files_and_cancellation() {
        let fixture = Fixture::new();
        std::fs::write(fixture.0.join("valid"), "text").unwrap();
        std::fs::write(fixture.0.join("invalid"), [0xff]).unwrap();
        for path in ["", "missing", "invalid", "."] {
            let error = fixture.load(&["valid", path]).err().unwrap();
            assert!(error.to_string().contains(&format!("{path:?}")));
        }
        assert!(
            load(
                vec!["valid".into()],
                fixture.0.clone(),
                Arc::new(AtomicBool::new(true))
            )
            .is_err()
        );
    }
}
