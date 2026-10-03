//! Input box: textarea setup and the key map layered over the textarea's defaults.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

pub enum InputAction {
    None,
    /// Enter was pressed on non-blank text. The caller decides whether to clear the
    /// input, since some states keep the draft.
    Submit(String),
    Stop,
    /// Shift+Tab: switch to the next approval mode.
    CycleMode,
}

/// Apply the configured input foreground while preserving the textarea's editing highlights.
pub fn new_textarea(theme: &crate::config::Theme) -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_style(theme.input);
    textarea.set_wrap_mode(WrapMode::Word);
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    // No block: the terminal layer draws the top rule and `> ` prompt around it.
    textarea
}

/// Identify the active attachment line only when completion can replace its entire path.
pub fn file_prefix<'a>(textarea: &'a TextArea<'_>) -> Option<(usize, &'a str)> {
    let cursor = textarea.cursor();
    let (row, column) = (cursor.0, cursor.1);
    let line = &textarea.lines()[row];
    (column == line.chars().count())
        .then(|| line.strip_prefix('@'))
        .flatten()
        .map(|prefix| (row, prefix))
}

/// Complete only the current attachment line, preserving every surrounding draft line.
pub fn complete_file(textarea: &mut TextArea, filename: &str) -> bool {
    if file_prefix(textarea).is_none() {
        return false;
    }
    textarea.cancel_selection();
    textarea.move_cursor(CursorMove::Head);
    textarea.delete_line_by_end();
    textarea.insert_str(format!("@{filename}"));
    true
}

/// Translate editing keys into submission and turn-control actions without trimming the draft.
pub fn handle_key(textarea: &mut TextArea, key: KeyEvent) -> InputAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter if key.modifiers.is_empty() => {
            let text = textarea.lines().join("\n");
            if text.trim().is_empty() {
                InputAction::None
            } else {
                InputAction::Submit(text)
            }
        }
        KeyCode::Esc => InputAction::Stop,
        KeyCode::BackTab => InputAction::CycleMode,
        // In raw mode ^J arrives as Ctrl+'j' (LF), distinct from Enter (CR).
        KeyCode::Char('j') if ctrl => {
            textarea.insert_newline();
            InputAction::None
        }
        // These deliberately override the textarea defaults: ^K and ^U kill to line
        // end/start (joining lines at the boundary), ^C and ^D do nothing.
        KeyCode::Char('k') if ctrl => {
            textarea.delete_line_by_end();
            InputAction::None
        }
        KeyCode::Char('u') if ctrl => {
            textarea.delete_line_by_head();
            InputAction::None
        }
        KeyCode::Char('c' | 'd') if ctrl => InputAction::None,
        _ => {
            textarea.input(key);
            InputAction::None
        }
    }
}

/// Insert bracketed-paste text as-is. Terminals commonly send newlines as `\r`.
pub fn paste(textarea: &mut TextArea, text: &str) {
    textarea.insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an unindented attachment line with its cursor at the end activates completion.
    #[test]
    fn file_completion_activation() {
        let mut input = TextArea::default();
        input.insert_str("Review\n@fi\n @literal");
        assert_eq!(file_prefix(&input), None);
        input.move_cursor(CursorMove::Jump(1, 2));
        assert_eq!(file_prefix(&input), None);
        input.move_cursor(CursorMove::End);
        assert_eq!(file_prefix(&input), Some((1, "fi")));
        input.move_cursor(CursorMove::Jump(0, 6));
        assert_eq!(file_prefix(&input), None);
    }

    /// Tab replacement preserves multiline text, filename spaces, and Unicode cursor positions.
    #[test]
    fn file_completion_replaces_only_active_line() {
        let mut input = TextArea::default();
        input.insert_str("before\n@雪\nafter\n");
        input.move_cursor(CursorMove::Jump(1, 2));
        assert_eq!(file_prefix(&input), Some((1, "雪")));
        assert!(complete_file(&mut input, "雪 file.txt"));
        assert_eq!(input.lines().join("\n"), "before\n@雪 file.txt\nafter\n");
        assert_eq!(input.cursor(), (1, "@雪 file.txt".chars().count()));
        input.move_cursor(CursorMove::Head);
        assert!(!complete_file(&mut input, "other"));
        assert_eq!(input.lines()[1], "@雪 file.txt");
    }

    /// Enter submits the literal typed attachment rather than any highlighted candidate.
    #[test]
    fn enter_keeps_typed_attachment() {
        let mut input = TextArea::default();
        input.insert_str("@fi");
        let action = handle_key(
            &mut input,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(matches!(action, InputAction::Submit(text) if text == "@fi"));
    }
}
