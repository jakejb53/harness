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
    // Whole-file text replacement: lines are unterminated so the target's
    // line ending and final-newline state can be applied at replace time.
    Replace {
        path: String,
        lines: Vec<String>,
    },
}

#[derive(Debug)]
pub struct Chunk {
    anchor: Option<Anchor>,
    lines: Vec<(char, String)>,
    eof: bool,
}

// A hunk anchor names an exact source line the hunk must follow. With `line`
// set, that 1-based input line must hold `text`, which disambiguates repeated
// anchor text while still failing fast when the model's view is stale.
#[derive(Debug)]
pub struct Anchor {
    line: Option<usize>,
    text: String,
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

// Consume the `+` body of Add/Replace up to the next operation header, leaving
// `i` on that header. Lines are returned without terminators.
fn content_lines(lines: &[&str], i: &mut usize) -> Result<Vec<String>, String> {
    let mut contents = Vec::new();
    while *i < lines.len() - 1 && !lines[*i].starts_with("*** ") {
        let text = lines[*i].strip_prefix('+').ok_or_else(|| {
            format!(
                "patch line {}: file content lines must start with +",
                *i + 1
            )
        })?;
        contents.push(text.to_owned());
        *i += 1;
    }
    Ok(contents)
}

// `line <digits>: <text>` is reserved for line-numbered anchors; any other header
// text is a plain anchor. Both parts are required so a stale view fails loudly
// instead of applying at a line the model never checked.
fn anchor(header: &str, line: usize) -> Result<Anchor, String> {
    let Some(rest) = header.strip_prefix("line ") else {
        return Ok(Anchor {
            line: None,
            text: header.to_owned(),
        });
    };
    let digits = rest.split(':').next().unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(Anchor {
            line: None,
            text: header.to_owned(),
        });
    }
    let number: usize = digits
        .parse()
        .map_err(|_| format!("patch line {line}: anchor line number {digits} is out of range"))?;
    if number == 0 {
        return Err(format!("patch line {line}: anchor line numbers start at 1"));
    }
    match rest[digits.len()..].strip_prefix(": ") {
        Some(text) if !text.is_empty() => Ok(Anchor {
            line: Some(number),
            text: text.to_owned(),
        }),
        _ => Err(format!(
            "patch line {line}: a line anchor needs the line's text after \"line {number}: \""
        )),
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
            // New files have no existing ending to inherit, so every line takes LF.
            let contents = content_lines(&lines, &mut i)?.into_iter().fold(
                String::new(),
                |mut contents, text| {
                    contents.push_str(&text);
                    contents.push('\n');
                    contents
                },
            );
            operations.push(Operation::Add {
                path: name,
                contents,
            });
        } else if let Some(name) = header.strip_prefix("*** Replace File: ") {
            let name = path(name, i + 1)?;
            i += 1;
            operations.push(Operation::Replace {
                path: name,
                lines: content_lines(&lines, &mut i)?,
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
                } else if let Some(header) = lines[i].strip_prefix("@@ ") {
                    i += 1;
                    Some(anchor(header, i)?)
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

// Use the same LF boundaries as read_file and patch parsing. A bare CR is data,
// including inside a quoted literal; retain terminators so untouched bytes survive.
pub(super) fn source_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

// A CR terminates a line only when paired with LF; standalone CR bytes remain literal text.
fn line_ending(line: &str) -> &'static str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
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
            .filter_map(|(i, line)| (line_text(line) == anchor.text).then_some(i))
            .collect();
        if let Some(line) = anchor.line {
            // The number selects the occurrence; the text proves the model's view is current.
            let index = line - 1;
            let found = if matches.is_empty() {
                "; the anchor text occurs nowhere in the file".to_owned()
            } else {
                format!("; the anchor text occurs at lines {}", locations(&matches))
            };
            if index >= lines.len() {
                return Err(format!(
                    "hunk {number}: anchor line {line} is beyond the file's {} lines{found}{}",
                    lines.len(),
                    candidate_context(lines, &matches)
                ));
            }
            if line_text(lines[index]) != anchor.text {
                return Err(format!(
                    "hunk {number}: anchor line {line} is {}, not {}{found}{}",
                    excerpt(line_text(lines[index])),
                    excerpt(&anchor.text),
                    candidate_context(lines, &[index]),
                ));
            }
            line
        } else {
            if matches.is_empty() || (expected.is_empty() && !chunk.eof && matches.len() > 1) {
                return Err(format!(
                    "hunk {number}: anchor {} yields {} insertion/search locations; candidates: {}{}",
                    excerpt(&anchor.text),
                    matches.len(),
                    locations(&matches),
                    candidate_context(lines, &matches)
                ));
            }
            matches[0] + 1
        }
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

// New text inherits the file's first terminator; a file without any takes LF.
fn source_ending(lines: &[&str]) -> &'static str {
    lines
        .iter()
        .map(|line| line_ending(line))
        .find(|ending| !ending.is_empty())
        .unwrap_or("\n")
}

// Final-newline state is preserved: the model's numbered view never shows it,
// so no text operation may change it. An empty original counts as terminated.
fn preserve_final_newline(original: &str, output: &mut [String]) {
    if !original.is_empty()
        && line_ending(original).is_empty()
        && let Some(last) = output.last_mut()
    {
        last.truncate(last.len() - line_ending(last).len());
    }
}

// Replace the whole text with `lines`, keeping only the file's ending and final-newline state.
pub fn replace(original: &str, lines: &[String]) -> Update {
    let ending = source_ending(&source_lines(original));
    let mut output: Vec<String> = lines.iter().map(|line| format!("{line}{ending}")).collect();
    preserve_final_newline(original, &mut output);
    Update {
        contents: output.concat(),
        regions: vec![(1, lines.len().max(1))],
    }
}

// Apply literal replacements while preserving untouched line bytes and the final-newline contract.
pub fn apply(original: &str, chunks: &[Chunk]) -> Result<Update, String> {
    let lines = source_lines(original);
    let ending = source_ending(&lines);
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
    preserve_final_newline(original, &mut output);
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

    // Exercise JSON decoding, raw controls, line boundaries, and hunk order together.
    #[test]
    fn quoted_controls_round_trip_through_json_and_patch() {
        let original = concat!(
            "match c {\n",
            r#"    '"' => out.push_str("\""),"#,
            "\n",
            r#"    '\\' => out.push_str("\\\\"),"#,
            "\n",
            "    '\n' => out.push_str(\"\\n\"),\n",
            "    '\r' => out.push_str(\"\\r\"),\n",
            "    '\t' => out.push_str(\"\\t\"),\n",
            "}"
        );
        let body = concat!(
            "@@ match c {\n",
            r#"-    '"' => out.push_str("\""),"#,
            "\n",
            r#"+    '"' => out.push_str("\\\""),"#,
            "\n",
            r#"     '\\' => out.push_str("\\\\"),"#,
            "\n",
            "-    '\n-' => out.push_str(\"\\n\"),\n",
            "-    '\r' => out.push_str(\"\\r\"),\n",
            "-    '\t' => out.push_str(\"\\t\"),\n",
            r#"+    '\n' => out.push_str("\\n"),"#,
            "\n",
            r#"+    '\r' => out.push_str("\\r"),"#,
            "\n",
            r#"+    '\t' => out.push_str("\\t"),"#,
            "\n",
            " }"
        );
        let expected = concat!(
            "match c {\n",
            r#"    '"' => out.push_str("\\\""),"#,
            "\n",
            r#"    '\\' => out.push_str("\\\\"),"#,
            "\n",
            r#"    '\n' => out.push_str("\\n"),"#,
            "\n",
            r#"    '\r' => out.push_str("\\r"),"#,
            "\n",
            r#"    '\t' => out.push_str("\\t"),"#,
            "\n",
            "}"
        );
        let patch = format!("*** Begin Patch\n*** Update File: f\n{body}\n*** End Patch");
        let encoded = serde_json::json!({"patch": patch}).to_string();
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let operations = parse(decoded["patch"].as_str().unwrap()).unwrap();
        let Operation::Update { chunks, .. } = &operations[0] else {
            panic!("expected update")
        };
        for ending in ["\n", "\r\n"] {
            let source = original.replace('\n', ending);
            let result = apply(&source, chunks).unwrap();
            assert_eq!(
                result.contents.as_bytes(),
                expected.replace('\n', ending).as_bytes()
            );
            assert_eq!(source_lines(&source).len(), 8);
        }
    }

    // The examples presented to the model must parse and produce the illustrated order and bytes.
    #[test]
    fn tool_description_examples_are_executable() {
        let description = crate::patch::DESCRIPTION;
        let start = description.find("*** Begin Patch\n").unwrap();
        let end =
            start + description[start..].find("*** End Patch").unwrap() + "*** End Patch".len();
        let example = &description[start..end];
        for patch in [
            example.to_owned(),
            example.replace("\n@@\n", "\n@@ fn example() {\n"),
        ] {
            let operations = parse(&patch).unwrap();
            let Operation::Update { chunks, .. } = &operations[0] else {
                panic!("expected update")
            };
            let result = apply(
                "fn example() {\n    old_call();\n    unchanged_call();\n}\n",
                chunks,
            )
            .unwrap();
            assert_eq!(
                result.contents,
                "fn example() {\n    new_call();\n    unchanged_call();\n}\n"
            );
        }
        let json_line = description
            .split_once("the JSON string representing that single added patch line is:\n")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap();
        let added: String = serde_json::from_str(json_line).unwrap();
        let result = update(
            "    old_call();\n",
            &format!("@@\n-    old_call();\n{added}"),
        )
        .unwrap();
        assert_eq!(
            result.contents,
            concat!(r#"    out.push_str("\\r");"#, "\n")
        );
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

    // An unchanged file retains every byte; CR-only content is one line, not several boundaries.
    #[test]
    fn unchanged_bytes_and_bare_cr_data() {
        for original in ["", "a", "a\r", "a\r\nb\nc\rd", "\r\n\r\n"] {
            let result = apply(original, &[]).unwrap();
            assert_eq!(result.contents, original);
            assert!(result.regions.is_empty());
        }
        assert_eq!(source_lines("a\rb\rc\r"), vec!["a\rb\rc\r"]);
        assert_eq!(line_text("a\rb\rc\r"), "a\rb\rc\r");
        assert_eq!(
            update("a\rb", "@@\n-a\rb\n+A\\rB").unwrap().contents,
            "A\\rB"
        );
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

    // A numbered anchor selects one occurrence of repeated text, for edits and insertions alike.
    #[test]
    fn line_anchor_selects_occurrence() {
        let original = "section\nx\nsection\nx\n";
        assert_eq!(
            update(original, "@@ line 3: section\n-x\n+y")
                .unwrap()
                .contents,
            "section\nx\nsection\ny\n"
        );
        assert_eq!(
            update(original, "@@ line 1: section\n+inserted")
                .unwrap()
                .contents,
            "section\ninserted\nx\nsection\nx\n"
        );
        assert_eq!(
            update(original, "@@ line 3: section\n+inserted")
                .unwrap()
                .contents,
            "section\nx\nsection\ninserted\nx\n"
        );
    }

    // A numbered anchor whose line holds other text is rejected with both texts and the real locations.
    #[test]
    fn line_anchor_rejects_stale_views() {
        let original = "section\nx\nsection\nx\n";
        let message = update(original, "@@ line 2: section\n-x\n+y")
            .err()
            .unwrap();
        assert!(
            message.contains("line 2 is \"x\", not \"section\""),
            "{message}"
        );
        assert!(message.contains("occurs at lines 1, 3"), "{message}");
        let message = update(original, "@@ line 9: section\n-x\n+y")
            .err()
            .unwrap();
        assert!(
            message.contains("line 9 is beyond the file's 4 lines"),
            "{message}"
        );
        let message = update(original, "@@ line 2: absent\n-x\n+y").err().unwrap();
        assert!(message.contains("occurs nowhere"), "{message}");
    }

    // Only the exact `line <digits>: text` form is numbered; the number and text are both required.
    #[test]
    fn line_anchor_syntax() {
        assert!(
            update("line 63: note\nx\n", "@@ line 63: note\n-x\n+y")
                .err()
                .unwrap()
                .contains("beyond the file's 2 lines")
        );
        assert_eq!(
            update("line six: note\nx\n", "@@ line six: note\n-x\n+y")
                .unwrap()
                .contents,
            "line six: note\ny\n"
        );
        for body in [
            "@@ line 0: x\n-x\n+y",
            "@@ line 2:\n-x\n+y",
            "@@ line 2: \n-x\n+y",
            "@@ line 2\n-x\n+y",
        ] {
            let error = update("a\nx\n", body).err().unwrap();
            assert!(error.starts_with("patch line 3:"), "{body:?}: {error}");
        }
    }

    // Replacement text takes the file's ending and final-newline state; nothing else survives.
    #[test]
    fn replace_preserves_ending_and_final_newline() {
        let lines = ["new".to_owned(), "text".to_owned()];
        assert_eq!(replace("a\nb\n", &lines).contents, "new\ntext\n");
        assert_eq!(replace("a\r\nb\n", &lines).contents, "new\r\ntext\r\n");
        assert_eq!(replace("a\nb", &lines).contents, "new\ntext");
        assert_eq!(replace("", &lines).contents, "new\ntext\n");
        assert_eq!(replace("a\n", &[]).contents, "");
        assert_eq!(replace("a\nb\n", &lines).regions, vec![(1, 2)]);
        assert_eq!(replace("a\n", &[]).regions, vec![(1, 1)]);
    }

    // Replace parses like Add but keeps lines unterminated for the engine to finish.
    #[test]
    fn parse_replace_operation() {
        let result = parse(
            "*** Begin Patch\n*** Replace File: f\n+one\n+\n+three\n*** Delete File: g\n*** End Patch",
        )
        .unwrap();
        assert!(
            matches!(&result[0], Operation::Replace { path, lines } if path == "f" && lines == &["one", "", "three"])
        );
        assert!(matches!(&result[1], Operation::Delete { .. }));
        assert!(
            parse("*** Begin Patch\n*** Replace File: f\nbare\n*** End Patch")
                .err()
                .unwrap()
                .contains("must start with +")
        );
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
