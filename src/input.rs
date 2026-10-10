//! Input box: textarea setup and the key map layered over the textarea's defaults.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::{TextArea, WrapMode};

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
