//! User-selected file reads, prepared without running the model's REPL.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::{Map, json};

use crate::session::{FunctionCall, ToolCall};

pub struct PreparedInjection {
    pub call: ToolCall,
    pub content: String,
}

/// Only a column-one `@` introduces a path; spaces and quoting remain literal.
pub fn paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix('@').map(str::to_owned))
        .collect()
}

/// Remove only attachment command lines; ordinary text remains an unsent draft verbatim.
pub fn remaining_text(text: &str) -> String {
    text.split('\n')
        .filter(|line| !line.starts_with('@'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Complete literal filenames from the launch directory without reading contents.
/// Paths containing a separator remain manual; completion never traverses directories.
pub fn completions(root: &std::path::Path, prefix: &str) -> Result<Vec<String>> {
    use std::os::unix::ffi::OsStrExt;

    if prefix.contains('/') {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("reading attachment directory {}", root.display()))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("reading attachment directory {}", root.display()))?;
        let name = entry.file_name();
        if !name.as_bytes().starts_with(prefix.as_bytes()) {
            continue;
        }
        let path = entry.path();
        let kind = entry
            .file_type()
            .with_context(|| format!("reading attachment file type {}", path.display()))?;
        let regular = if kind.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(metadata) => metadata.is_file(),
                // A dangling link cannot be attached; other inspection failures are errors.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading attachment symlink {}", path.display()));
                }
            }
        } else {
            kind.is_file()
        };
        if !regular {
            continue;
        }
        let name = name
            .to_str()
            .with_context(|| format!("attachment filename is not UTF-8: {path:?}"))?;
        // A completed filename must stay on one command line without line-ending normalization.
        if name.contains(['\n', '\r']) {
            bail!("attachment filename contains a newline: {path:?}");
        }
        candidates.push(name.to_owned());
    }
    candidates.sort();
    Ok(candidates)
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
    let relative = relative_path(std::path::Path::new(path), root)?;
    let relative = relative.to_str().context("attachment path is not UTF-8")?;
    // JSON string escaping also yields a Python string literal. Reading bytes
    // avoids Python's newline translation, preserving the exact supplied result.
    let literal = serde_json::to_string(relative)?;
    let code = format!(
        "from pathlib import Path\nprint(Path({literal}).read_bytes().decode('utf-8'), end='')"
    );
    Ok(PreparedInjection {
        call: ToolCall {
            id: crate::tool_call_id::generate(crate::tool_call_id::INJECTED_FILE, &[])?,
            kind: "function".into(),
            function: FunctionCall {
                name: "REPL".into(),
                arguments: json!({ "code": code }).to_string(),
                extra: Map::new(),
            },
            extra: Map::new(),
        },
        content,
    })
}

/// Keep host-root details out of synthetic code while preserving symlink-sensitive `..` paths.
fn relative_path(path: &std::path::Path, root: &std::path::Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Ok(path.to_owned());
    }
    if !root.is_absolute() {
        bail!("attachment launch directory must be absolute");
    }
    let target: Vec<_> = path.components().collect();
    let base: Vec<_> = root.components().collect();
    let shared = target.iter().zip(&base).take_while(|(a, b)| a == b).count();
    let mut relative = PathBuf::new();
    for _ in &base[shared..] {
        relative.push("..");
    }
    for component in &target[shared..] {
        relative.push(component.as_os_str());
    }
    Ok(relative)
}

/// Recover the relative filename from our generated code without evaluating Python.
/// Marked calls with another schema are errors rather than silently hidden attachments.
pub fn filename(call: &ToolCall) -> Result<String> {
    if call.kind != "function" || call.function.name != "REPL" {
        bail!(
            "invalid file injection {:?}: expected a REPL function call",
            call.id
        );
    }
    let arguments: serde_json::Value = serde_json::from_str(&call.function.arguments)
        .with_context(|| format!("invalid file injection {:?}: malformed arguments", call.id))?;
    let literal = arguments
        .get("code")
        .and_then(serde_json::Value::as_str)
        .and_then(|code| code.strip_prefix("from pathlib import Path\nprint(Path("))
        .and_then(|code| code.strip_suffix(").read_bytes().decode('utf-8'), end='')"))
        .with_context(|| {
            format!(
                "invalid file injection {:?}: unexpected file-reading code",
                call.id
            )
        })?;
    let path: String = serde_json::from_str(literal)
        .with_context(|| format!("invalid file injection {:?}: malformed filename", call.id))?;
    if path.is_empty() || std::path::Path::new(&path).is_absolute() {
        bail!(
            "invalid file injection {:?}: expected a relative filename",
            call.id
        );
    }
    Ok(path)
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
    use std::sync::atomic::AtomicU64;

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

    /// Attachment commands leave ordinary text unsent, including whitespace and literal at signs.
    #[test]
    fn attachment_lines_never_become_message_text() {
        assert_eq!(remaining_text("@file"), "");
        assert_eq!(remaining_text("@one\n@two"), "");
        assert_eq!(remaining_text("@one\nReview this\n@two"), "Review this");
        assert_eq!(
            remaining_text("first\n@file\n\nlast \n"),
            "first\n\nlast \n"
        );
        assert_eq!(
            remaining_text(" @literal\ninline @file\n@real"),
            " @literal\ninline @file"
        );
        assert_eq!(remaining_text("@a file with spaces\n"), "");
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
            assert_eq!(filename(&injection.call).unwrap(), "file");
            assert_eq!(injection.content, content);
            let marker = crate::tool_call_id::decode(&injection.call.id).unwrap();
            assert_eq!(marker.kind, crate::tool_call_id::INJECTED_FILE);
            assert!(marker.data.is_empty());
            assert_eq!(injection.call.function.name, "REPL");
        }
    }

    /// Absolute input stays local; both input spellings produce the same relative Python path.
    #[test]
    fn spaces_quotes_unicode_and_absolute_paths() {
        let fixture = Fixture::new();
        let name = "a 'quoted' \"雪\" ";
        let absolute = fixture.0.join(name);
        std::fs::write(&absolute, "hello\r\n").unwrap();
        let loaded = fixture.load(&[name, absolute.to_str().unwrap()]).unwrap();
        assert_eq!(filename(&loaded[0].call).unwrap(), name);
        assert_eq!(filename(&loaded[1].call).unwrap(), name);
        assert_eq!(loaded[0].content, "hello\r\n");
        let arguments: serde_json::Value =
            serde_json::from_str(&loaded[0].call.function.arguments).unwrap();
        let literal = serde_json::to_string(name).unwrap();
        assert_eq!(
            arguments["code"],
            format!(
                "from pathlib import Path\nprint(Path({literal}).read_bytes().decode('utf-8'), end='')"
            )
        );
        assert_eq!(
            loaded[0].call.function.arguments,
            loaded[1].call.function.arguments
        );
        for injection in &loaded {
            let sent = serde_json::to_string(&injection.call).unwrap();
            assert!(!sent.contains(fixture.0.to_str().unwrap()));
            assert!(injection.call.extra.is_empty());
            assert!(injection.call.function.extra.is_empty());
        }
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
        for injection in &loaded {
            assert_eq!(injection.content, content);
        }
        let ids: std::collections::HashSet<_> =
            loaded.iter().map(|injection| &injection.call.id).collect();
        assert_eq!(ids.len(), 3);
        for (injection, relative) in
            loaded
                .iter()
                .zip(["../outside.txt", "../outside.txt", "linked.txt"])
        {
            let code: serde_json::Value =
                serde_json::from_str(&injection.call.function.arguments).unwrap();
            assert!(
                code["code"]
                    .as_str()
                    .unwrap()
                    .contains(&serde_json::to_string(relative).unwrap())
            );
            assert!(
                !injection
                    .call
                    .function
                    .arguments
                    .contains(fixture.0.to_str().unwrap())
            );
        }
    }

    /// Relative conversion must not collapse parent components through symlinks.
    #[test]
    fn relative_code_paths_preserve_parent_semantics() {
        let root = std::path::Path::new("/repo/work");
        for (path, expected) in [
            ("link/../file", "link/../file"),
            ("/repo/work/link/../file", "link/../file"),
            ("/repo/other/file", "../other/file"),
            ("/elsewhere/file", "../../elsewhere/file"),
        ] {
            assert_eq!(
                relative_path(std::path::Path::new(path), root).unwrap(),
                PathBuf::from(expected)
            );
        }
    }

    /// Completion includes hidden files and regular-file links, but never directory contents.
    #[test]
    fn completion_filters_and_sorts_literal_filenames() {
        let fixture = Fixture::new();
        for name in ["zebra", "alpha", "Apple", ".hidden", "a space", "雪"] {
            std::fs::write(fixture.0.join(name), [0xff]).unwrap();
        }
        std::fs::create_dir(fixture.0.join("nested")).unwrap();
        std::fs::write(fixture.0.join("nested/deeper"), "nested contents").unwrap();
        std::os::unix::fs::symlink("alpha", fixture.0.join("linked")).unwrap();
        std::os::unix::fs::symlink("nested", fixture.0.join("directory-link")).unwrap();
        std::os::unix::fs::symlink("missing", fixture.0.join("dangling")).unwrap();
        assert_eq!(
            completions(&fixture.0, "").unwrap(),
            [
                ".hidden", "Apple", "a space", "alpha", "linked", "zebra", "雪"
            ]
        );
        assert_eq!(completions(&fixture.0, "a").unwrap(), ["a space", "alpha"]);
        assert_eq!(completions(&fixture.0, "A").unwrap(), ["Apple"]);
        assert_eq!(completions(&fixture.0, "a ").unwrap(), ["a space"]);
        assert_eq!(completions(&fixture.0, "雪").unwrap(), ["雪"]);
        assert_eq!(completions(&fixture.0, ".").unwrap(), [".hidden"]);
        assert!(completions(&fixture.0, "absent").unwrap().is_empty());
        for prefix in ["nested/", "../", "/", "nested/deeper"] {
            assert!(completions(&fixture.0, prefix).unwrap().is_empty());
        }
    }

    /// Enumeration and unrepresentable matching names produce actionable errors.
    #[test]
    fn completion_reports_directory_and_filename_errors() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("missing");
        let error = completions(&missing, "").unwrap_err().to_string();
        assert!(error.contains(missing.to_str().unwrap()));
        std::fs::write(fixture.0.join("line\nbreak"), "text").unwrap();
        let error = completions(&fixture.0, "line").unwrap_err().to_string();
        assert!(error.contains("newline"));
        assert!(error.contains("line\\nbreak"));
        // macOS rejects non-UTF-8 filenames at creation, before completion can inspect them.
        #[cfg(not(target_os = "macos"))]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid = std::ffi::OsString::from_vec(b"invalid-\xff".to_vec());
            std::fs::write(fixture.0.join(&invalid), "text").unwrap();
            let error = completions(&fixture.0, "invalid").unwrap_err().to_string();
            assert!(error.contains("not UTF-8"));
            assert!(error.contains("invalid-"));
        }
        assert!(completions(&fixture.0, "unrelated").unwrap().is_empty());
        std::os::unix::fs::symlink("loop", fixture.0.join("loop")).unwrap();
        let error = completions(&fixture.0, "loop").unwrap_err().to_string();
        assert!(error.contains("reading attachment symlink"));
        assert!(error.contains("loop"));
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
