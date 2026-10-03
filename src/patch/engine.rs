//! Literal, in-memory patch parsing and application. Filesystem policy belongs to the caller.
//! Format reference: https://github.com/openai/codex/tree/main/codex-rs/apply-patch
//! This implementation is independent; no upstream source is copied.

#[derive(Debug)]
pub enum Operation {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

#[derive(Debug)]
pub struct Chunk {
    anchor: Option<String>,
    lines: Vec<(char, String)>,
    eof: bool,
}

pub struct Update {
    pub contents: String,
    pub regions: Vec<(usize, usize)>,
}

// Reject empty paths here; containment and path alias checks require filesystem knowledge.
fn path(value: &str, line: usize) -> Result<String, String> {
    if value.is_empty() {
        Err(format!("patch line {line}: missing file path"))
    } else {
        Ok(value.to_owned())
    }
}

// Parse patch syntax while leaving replacement text literal and accepting harmless envelope whitespace.
pub fn parse(patch: &str) -> Result<Vec<Operation>, String> {
    let lines: Vec<&str> = patch
        .trim()
        .split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err("patch must start with *** Begin Patch and end with *** End Patch".into());
    }
    let mut operations = Vec::new();
    let mut i = 1;
    while i < lines.len() - 1 {
        let header = lines[i];
        if let Some(name) = header.strip_prefix("*** Add File: ") {
            let name = path(name, i + 1)?;
            i += 1;
            let mut contents = String::new();
            while i < lines.len() - 1 && !lines[i].starts_with("*** ") {
                let text = lines[i].strip_prefix('+').ok_or_else(|| {
                    format!("patch line {}: added file lines must start with +", i + 1)
                })?;
                contents.push_str(text);
                contents.push('\n');
                i += 1;
            }
            operations.push(Operation::Add {
                path: name,
                contents,
            });
        } else if let Some(name) = header.strip_prefix("*** Delete File: ") {
            operations.push(Operation::Delete {
                path: path(name, i + 1)?,
            });
            i += 1;
        } else if let Some(name) = header.strip_prefix("*** Update File: ") {
            let name = path(name, i + 1)?;
            i += 1;
            let move_to = if let Some(destination) = lines[i].strip_prefix("*** Move to: ") {
                let destination = path(destination, i + 1)?;
                i += 1;
                Some(destination)
            } else {
                None
            };
            let mut chunks = Vec::new();
            while i < lines.len() - 1 && !lines[i].starts_with("*** ") {
                let anchor = if lines[i] == "@@" {
                    i += 1;
                    None
                } else if let Some(anchor) = lines[i].strip_prefix("@@ ") {
                    i += 1;
                    Some(anchor.to_owned())
                } else if chunks.is_empty() {
                    None
                } else {
                    return Err(format!("patch line {}: expected @@ hunk header", i + 1));
                };
                let mut chunk = Chunk {
                    anchor,
                    lines: Vec::new(),
                    eof: false,
                };
                while i < lines.len() - 1
                    && !lines[i].starts_with("@@")
                    && !lines[i].starts_with("*** ")
                {
                    let line = lines[i];
                    let kind = line.chars().next().unwrap_or(' ');
                    if !matches!(kind, ' ' | '+' | '-') {
                        return Err(format!(
                            "patch line {}: expected space, +, or - prefix",
                            i + 1
                        ));
                    }
                    chunk
                        .lines
                        .push((kind, line.get(1..).unwrap_or("").to_owned()));
                    i += 1;
                }
                if lines[i] == "*** End of File" {
                    chunk.eof = true;
                    i += 1;
                }
                chunks.push(chunk);
            }
            operations.push(Operation::Update {
                path: name,
                move_to,
                chunks,
            });
        } else {
            return Err(format!(
                "patch line {}: unrecognized operation {header:?}",
                i + 1
            ));
        }
    }
    Ok(operations)
}

// Split all supported line endings without normalizing source bytes or treating CRLF as two lines.
pub(super) fn source_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' || bytes[i] == b'\n' {
            if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
                i += 1;
            }
            lines.push(&text[start..=i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

// Recognize only the final physical terminator, including classic CR-only files.
fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else if line.ends_with('\r') {
        "\r"
    } else {
        ""
    }
}

// Preserve each source line's terminator separately from the literal text used for matching.
pub(super) fn line_text(line: &str) -> &str {
    &line[..line.len() - line_ending(line).len()]
}

// Bound diagnostics while retaining useful coordinates for disambiguating a rejected hunk.
fn locations(matches: &[usize]) -> String {
    let mut result = matches
        .iter()
        .take(10)
        .map(|position| (position + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if matches.len() > 10 {
        result.push_str(&format!(", … ({} matches total)", matches.len()));
    }
    result
}

// Limit echoed source text by Unicode characters without cutting UTF-8 or hiding truncation.
fn excerpt(text: &str) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    format!("{result:?}")
}

// Show bounded neighboring source lines to make ambiguous or stale matches repairable in one turn.
fn candidate_context(lines: &[&str], candidates: &[usize]) -> String {
    let mut shown = Vec::new();
    let mut result = String::new();
    for &candidate in candidates.iter().take(3) {
        for (index, line) in lines
            .iter()
            .enumerate()
            .take((candidate + 3).min(lines.len()))
            .skip(candidate.saturating_sub(1))
        {
            if !shown.contains(&index) {
                result.push_str(&format!("\n  {}: {}", index + 1, excerpt(line_text(line))));
                shown.push(index);
            }
        }
    }
    result
}

// Resolve the complete target against the original snapshot; repeated anchors may still yield one target.
fn locate(lines: &[&str], chunk: &Chunk, number: usize) -> Result<usize, String> {
    let expected: Vec<&str> = chunk
        .lines
        .iter()
        .filter_map(|(kind, text)| (*kind != '+').then_some(text.as_str()))
        .collect();
    let start = if let Some(anchor) = &chunk.anchor {
        let matches: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| (line_text(line) == anchor).then_some(i))
            .collect();
        if matches.is_empty() || (expected.is_empty() && !chunk.eof && matches.len() > 1) {
            return Err(format!(
                "hunk {number}: anchor {} yields {} insertion/search locations; candidates: {}{}",
                excerpt(anchor),
                matches.len(),
                locations(&matches),
                candidate_context(lines, &matches)
            ));
        }
        matches[0] + 1
    } else {
        0
    };
    if expected.is_empty() {
        if chunk.eof || chunk.anchor.is_none() {
            return Ok(lines.len());
        }
        return Ok(start);
    }
    let matches: Vec<usize> = (start..=lines.len())
        .filter(|&i| {
            i + expected.len() <= lines.len()
                && (!chunk.eof || i + expected.len() == lines.len())
                && expected
                    .iter()
                    .zip(&lines[i..])
                    .all(|(wanted, actual)| *wanted == line_text(actual))
        })
        .collect();
    match matches.as_slice() {
        [position] => Ok(*position),
        [] => {
            let nearby: Vec<usize> = lines
                .iter()
                .enumerate()
                .skip(start)
                .filter_map(|(i, line)| (line_text(line).trim() == expected[0].trim()).then_some(i))
                .collect();
            let mut message = format!(
                "hunk {number}: no exact match for {} context/removal lines beginning with {}",
                expected.len(),
                excerpt(expected[0])
            );
            if !nearby.is_empty() {
                message.push_str(&format!(
                    "; possible first-line locations: {} (whitespace differences are not accepted)",
                    locations(&nearby)
                ));
                message.push_str(&candidate_context(lines, &nearby));
            } else {
                message.push_str("; start of searched region:");
                message.push_str(&candidate_context(lines, &[start]));
            }
            Err(message)
        }
        _ => Err(format!(
            "hunk {number}: ambiguous exact match at lines {}; add distinguishing context{}",
            locations(&matches),
            candidate_context(lines, &matches)
        )),
    }
}

struct Edit {
    start: usize,
    end: usize,
    added: Vec<String>,
    hunk: usize,
}

// Build independent changed intervals so that shared unchanged hunk context remains legal.
fn edits(lines: &[&str], chunks: &[Chunk]) -> Result<Vec<Edit>, String> {
    let mut edits = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let mut cursor = locate(lines, chunk, index + 1)?;
        let mut pending: Option<Edit> = None;
        for (kind, text) in &chunk.lines {
            if *kind == ' ' {
                if let Some(edit) = pending.take() {
                    edits.push(edit);
                }
                cursor += 1;
            } else {
                let edit = pending.get_or_insert_with(|| Edit {
                    start: cursor,
                    end: cursor,
                    added: Vec::new(),
                    hunk: index + 1,
                });
                if *kind == '-' {
                    cursor += 1;
                    edit.end = cursor;
                } else {
                    edit.added.push(text.clone());
                }
            }
        }
        if let Some(edit) = pending {
            edits.push(edit);
        }
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));
    for pair in edits.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if b.start < a.end || (a.start == a.end && a.start == b.start) {
            return Err(format!(
                "hunks {} and {} have overlapping changes at original line {}",
                a.hunk,
                b.hunk,
                b.start + 1
            ));
        }
    }
    Ok(edits)
}

// Apply literal replacements while preserving untouched line bytes and the final-newline contract.
pub fn apply(original: &str, chunks: &[Chunk]) -> Result<Update, String> {
    let lines = source_lines(original);
    let ending = lines
        .iter()
        .map(|line| line_ending(line))
        .find(|ending| !ending.is_empty())
        .unwrap_or("\n");
    let changes = edits(&lines, chunks)?;
    let mut output: Vec<String> = Vec::new();
    let mut cursor = 0;
    let mut regions = Vec::new();
    for edit in changes {
        output.extend(
            lines[cursor..edit.start]
                .iter()
                .map(|line| (*line).to_owned()),
        );
        let start = output.len() + 1;
        output.extend(edit.added.iter().map(|line| format!("{line}{ending}")));
        regions.push((start, output.len().max(start)));
        cursor = edit.end;
    }
    output.extend(lines[cursor..].iter().map(|line| (*line).to_owned()));
    let count = output.len();
    // Appending after an unterminated source line requires a separator at that changed seam.
    for line in output.iter_mut().take(count.saturating_sub(1)) {
        if line_ending(line).is_empty() {
            line.push_str(ending);
        }
    }
    if !original.is_empty()
        && line_ending(original).is_empty()
        && let Some(last) = output.last_mut()
    {
        last.truncate(last.len() - line_ending(last).len());
    }
    for (start, end) in &mut regions {
        *start = (*start).min(count.max(1));
        *end = (*end).min(count.max(1));
    }
    Ok(Update {
        contents: output.concat(),
        regions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise the public parser and application boundary together for update fixtures.
    fn update(original: &str, body: &str) -> Result<Update, String> {
        let operations = parse(&format!(
            "*** Begin Patch\n*** Update File: f\n{body}\n*** End Patch"
        ))?;
        let Operation::Update { chunks, .. } = &operations[0] else {
            panic!("expected update")
        };
        apply(original, chunks)
    }

    // Literal backslashes must survive exactly; neither regex nor string replacement interprets them.
    #[test]
    fn literal_text_and_final_newline() {
        let result = update("before\nold", "@@\n before\n-old\n+\\n\\t\\1").unwrap();
        assert_eq!(result.contents, "before\n\\n\\t\\1");
        assert_eq!(result.regions, vec![(2, 2)]);
    }

    // New lines use the first source terminator while untouched mixed endings remain byte exact.
    #[test]
    fn mixed_line_endings() {
        assert_eq!(
            update("a\r\nb\nc\r\n", "@@\n-b\n+B").unwrap().contents,
            "a\r\nB\r\nc\r\n"
        );
    }

    // Context may overlap because it is verified against one snapshot and is not itself edited.
    #[test]
    fn shared_context_multiple_hunks() {
        assert_eq!(
            update("a\nb\nc\n", "@@\n-a\n+A\n b\n@@\n b\n-c\n+C")
                .unwrap()
                .contents,
            "A\nb\nC\n"
        );
    }

    // Ambiguity is an error even when the first match would be convenient to apply.
    #[test]
    fn ambiguity_and_anchor() {
        assert!(
            update("x\na\nx\n", "@@\n-x\n+y")
                .err()
                .unwrap()
                .contains("ambiguous")
        );
        assert_eq!(
            update("x\na\nx\n", "@@ a\n-x\n+y").unwrap().contents,
            "x\na\ny\n"
        );
    }

    // Two hunks must not silently overwrite the same original source lines.
    #[test]
    fn overlapping_changes_fail() {
        assert!(
            update("a\nb\n", "@@\n-a\n+A\n@@\n-a\n+Z")
                .err()
                .unwrap()
                .contains("overlapping")
        );
    }

    // Whitespace and punctuation remain semantic bytes, rather than fuzzy matching hints.
    #[test]
    fn no_relaxed_matching() {
        assert!(update("  x\n", "@@\n-x\n+y").is_err());
        assert!(update("‘x’\n", "@@\n-'x'\n+y").is_err());
    }

    // Unanchored additions append; anchored additions identify the boundary after the anchor.
    #[test]
    fn insertion_boundaries() {
        assert_eq!(update("", "@@\n+a").unwrap().contents, "a\n");
        assert_eq!(update("a\n", "@@\n+b").unwrap().contents, "a\nb\n");
        assert_eq!(update("a\nz\n", "@@ a\n+b").unwrap().contents, "a\nb\nz\n");
        assert!(update("a\na\n", "@@ a\n+b").is_err());
        assert_eq!(
            update("a", "@@\n+b\n*** End of File").unwrap().contents,
            "a\nb"
        );
    }

    // File operations preserve literal paths and reject malformed envelopes before application.
    #[test]
    fn parse_file_operations() {
        let result = parse("*** Begin Patch\n*** Add File: new\n+x\n*** Delete File: old\n*** Update File: a\n*** Move to: b\n*** End Patch").unwrap();
        assert!(matches!(&result[0], Operation::Add { contents, .. } if contents == "x\n"));
        assert!(
            matches!(&result[2], Operation::Update { move_to: Some(name), chunks, .. } if name == "b" && chunks.is_empty())
        );
        assert!(parse("*** Begin Patch\n*** End Patch").unwrap().is_empty());
    }

    // Rename-only operations must preserve every source byte, including all newline conventions.
    #[test]
    fn rename_only_and_cr_endings() {
        for original in ["", "a", "a\r", "a\r\nb\nc\rd", "\r\n\r\n"] {
            let result = apply(original, &[]).unwrap();
            assert_eq!(result.contents, original);
            assert!(result.regions.is_empty());
        }
        assert_eq!(
            update("a\rb\rc\r", "@@\n-b\n+B").unwrap().contents,
            "a\rB\rc\r"
        );
        assert_eq!(update("a\rb", "@@\n-b\n+B").unwrap().contents, "a\rB");
    }

    // Invalid boundary syntax is rejected rather than indexing beyond the patch envelope.
    #[test]
    fn malformed_boundaries_do_not_panic() {
        for patch in [
            "",
            "*** Begin Patch",
            "*** End Patch",
            "*** Begin Patch\n*** Update File: f\n*** Move to: \n*** End Patch",
            "*** Begin Patch\n*** Update File: f\n@@\n💡bad\n*** End Patch",
            "*** Begin Patch\n*** End Patch\n*** End Patch",
        ] {
            assert!(parse(patch).is_err(), "accepted malformed patch: {patch:?}");
        }
    }

    // Failed matching should return enough bounded source context to repair the next request.
    #[test]
    fn diagnostics_include_source_context() {
        let message = update("first\nx\nsecond\nx\n", "@@\n-x\n+y").err().unwrap();
        assert!(message.contains("1: \"first\""));
        assert!(message.contains("3: \"second\""));
        let message = update("  x\nneighbor\n", "@@\n-x\n+y").err().unwrap();
        assert!(message.contains("1: \"  x\""));
        assert!(message.contains("2: \"neighbor\""));
    }

    // Anchor text can repeat when the complete old-line target after it remains unique.
    #[test]
    fn repeated_anchors_use_complete_target() {
        assert_eq!(
            update(
                "section\nfirst\nsection\nsecond\n",
                "@@ section\n-second\n+changed"
            )
            .unwrap()
            .contents,
            "section\nfirst\nsection\nchanged\n"
        );
        assert!(update("section\nx\nsection\nx\n", "@@ section\n-x\n+y").is_err());
    }

    // Harmless envelope whitespace and empty context spelling do not alter literal authored bytes.
    #[test]
    fn envelope_whitespace_blank_context_and_noops() {
        let operations =
            parse(" \n\t*** Begin Patch\n*** Update File: f\n@@\n a\n\n-b\n+B\n*** End Patch\n \t")
                .unwrap();
        let Operation::Update { chunks, .. } = &operations[0] else {
            panic!("expected update")
        };
        assert_eq!(apply("a\n\nb\n", chunks).unwrap().contents, "a\n\nB\n");
        for body in ["@@", "@@\n a", "@@\n*** End of File"] {
            let result = update("a\n", body).unwrap();
            assert_eq!(result.contents, "a\n");
            assert!(result.regions.is_empty());
        }
        assert!(
            parse(" \n*** Begin Patch\n*** End Patch\n ")
                .unwrap()
                .is_empty()
        );
        let operations = parse("*** Begin Patch\n*** Update File: f\n*** End Patch").unwrap();
        let Operation::Update { chunks, .. } = &operations[0] else {
            panic!("expected update")
        };
        assert!(chunks.is_empty());
        assert_eq!(apply("a\n", chunks).unwrap().contents, "a\n");
    }
}
