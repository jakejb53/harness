//! The `reasoning()` approval exemption: code that is exactly one `reasoning(...)`
//! call on a single string literal is answered with `(no output)` without approval
//! and without running.
//!
//! Honesty rule: that answer claims what `reasoning()` produces, so it relies on
//! `replib/reasoning.py` defining it as a no-op that prints nothing.

/// The result of every exact `reasoning("…")` call: what `reasoning()` produces
/// (nothing), as the REPL reports it.
pub const RESULT: &str = "(no output)";

/// Code that is exactly one `reasoning(...)` call on a single Python string literal,
/// so nothing else can run. f-strings (which evaluate expressions), bytes, implicit
/// concatenation, keyword arguments, and any other code alongside don't qualify.
pub fn is_reasoning_call(code: &str) -> bool {
    code.trim()
        .strip_prefix("reasoning(")
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|argument| is_string_literal(argument.trim()))
}

/// One complete Python string literal: an optional `r` or `u` prefix, then single,
/// double, or triple quotes. The first unescaped closing quote must end the text.
fn is_string_literal(text: &str) -> bool {
    let body = text.strip_prefix(['r', 'R', 'u', 'U']).unwrap_or(text);
    let quote = ["\"\"\"", "'''", "\"", "'"]
        .into_iter()
        .find(|quote| body.starts_with(quote));
    let Some(quote) = quote else {
        return false;
    };
    let content = &body[quote.len()..];
    let mut chars = content.char_indices();
    while let Some((position, c)) = chars.next() {
        match c {
            // Escapes the next character, including a quote or a line break, in raw
            // strings too (where the backslash is kept but still can't end the string).
            '\\' => {
                if chars.next().is_none() {
                    return false;
                }
            }
            '\n' if quote.len() == 1 => return false,
            _ if content[position..].starts_with(quote) => {
                return position + quote.len() == content.len();
            }
            _ => {}
        }
    }
    false
}
