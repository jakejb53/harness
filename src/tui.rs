//! Terminal layer: a full-screen UI on the alternate screen. The app draws the whole
//! screen each frame: the transcript view (`transcript::View`) above an input region
//! of a top rule, the `> ` prompt and input box, and footer rows (the status line, or
//! the command completion list in its place).
//!
//! The mouse is captured, so the app handles wheel scrolling, selection, and copy
//! (OSC 52). Leaving the alternate screen on exit restores the terminal as it was.

use std::io::{self, Stdout, Write};

use crossterm::{
    Command,
    clipboard::CopyToClipboard,
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste},
    queue,
    terminal::{
        self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block as Border, Borders, Clear, Widget};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::Theme;
use crate::transcript::{Block, View};

/// Prompt marker at the start of the input box.
const PROMPT: &str = "> ";

/// Mouse reporting for what the app uses: presses, releases, and the wheel (1000),
/// motion only while a button is held, for dragging (1002), in SGR encoding (1006).
/// Unlike crossterm's `EnableMouseCapture`, it leaves out 1003, which reports every
/// movement and would redraw the screen on each. `DisableMouseCapture` turns it off.
struct EnableMouseButtons;

impl Command for EnableMouseButtons {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }
}

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    messages: MessageGeometry,
}

/// Rows are a compact projection of the saved session, not request-time messages.
pub struct MessageOverlay<'a> {
    pub entries: &'a [String],
    pub scroll: usize,
}

/// Mouse routing and scroll bounds belong to the displayed frame.
#[derive(Default)]
struct MessageGeometry {
    area: Rect,
    scroll: usize,
    max_scroll: usize,
}

impl MessageGeometry {
    /// Borders absorb mouse events too, preventing selection of covered transcript text.
    fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains((x, y).into())
    }
}

impl Tui {
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        terminal::enable_raw_mode()?;
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = restore(&mut io::stdout());
                return Err(error);
            }
        };
        // Construct before the remaining fallible calls so `Drop` restores the
        // terminal if setup fails partway.
        let mut tui = Tui {
            terminal,
            messages: MessageGeometry::default(),
        };
        let out = tui.terminal.backend_mut();
        queue!(
            out,
            EnterAlternateScreen,
            EnableMouseButtons,
            EnableBracketedPaste,
            Hide
        )?;
        out.flush()?;
        // Start from a blank screen; ratatui's buffers assume it.
        tui.terminal.clear()?;
        Ok(tui)
    }

    /// Draw a frame as one synchronized update. Only cells that changed since the
    /// last frame are written; a resize clears and redraws everything.
    pub fn draw(
        &mut self,
        blocks: &[Block],
        view: &mut View,
        textarea: &TextArea,
        footer: &[Line],
        theme: &Theme,
        overlay: MessageOverlay<'_>,
    ) -> io::Result<()> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let mut messages = MessageGeometry::default();
        let drawn = self
            .terminal
            .draw(|frame| {
                let area = frame.area();
                let transcript = render(
                    frame.buffer_mut(),
                    area,
                    blocks,
                    view,
                    textarea,
                    footer,
                    theme,
                );
                messages = render_messages(frame.buffer_mut(), transcript, &overlay, theme);
            })
            .map(|_| ());
        if drawn.is_ok() {
            self.messages = messages;
        }
        let out = self.terminal.backend_mut();
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        drawn
    }

    /// Route all mouse events inside the displayed overlay away from the transcript.
    pub fn over_messages(&self, x: u16, y: u16) -> bool {
        self.messages.contains(x, y)
    }

    /// Use the frame's effective scroll after clamping to the available rows.
    pub fn message_scroll(&self) -> usize {
        self.messages.scroll
    }

    /// Bound wheel scrolling by the latest frame's available message rows.
    pub fn message_max_scroll(&self) -> usize {
        self.messages.max_scroll
    }

    /// Put `text` on the system clipboard (OSC 52). Terminals without OSC 52 support
    /// ignore it.
    pub fn copy(&mut self, text: &str) -> io::Result<()> {
        let out = self.terminal.backend_mut();
        queue!(out, CopyToClipboard::to_clipboard_from(text))?;
        out.flush()
    }

    pub fn exit(&mut self) -> io::Result<()> {
        restore(self.terminal.backend_mut())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore(&mut io::stdout());
    }
}

/// Lay out the screen: the transcript fills everything above the input region.
fn render(
    buf: &mut Buffer,
    area: Rect,
    blocks: &[Block],
    view: &mut View,
    textarea: &TextArea,
    footer: &[Line],
    theme: &Theme,
) -> Rect {
    let (width, height) = (area.width, area.height);
    if width == 0 || height == 0 {
        return Rect::default();
    }
    // Text sits right of the `> ` prompt; wrapped and ^J lines align under it.
    let text_x = PROMPT.len() as u16;
    let text_width = width.saturating_sub(text_x).max(1);
    let max_input_rows = (height / 2).saturating_sub(2).max(1);
    let input_rows = wrapped_rows(textarea, text_width).clamp(1, max_input_rows);
    // At least one footer row (the status line); more push the region up, leaving
    // room for the top rule and one input row.
    let footer_rows = (footer.len() as u16).clamp(1, height.saturating_sub(2).max(1));
    // Top rule + input rows + footer rows, never taller than the screen.
    let region_height = (input_rows + 1 + footer_rows).min(height);
    let region_top = height - region_height;

    view.render(blocks, Rect::new(0, 0, width, region_top), buf);

    Border::default()
        .borders(Borders::TOP)
        .border_style(theme.status)
        .render(Rect::new(0, region_top, width, 1), buf);
    let text_rows = region_height.saturating_sub(1 + footer_rows);
    if text_rows > 0 {
        buf.set_string(0, region_top + 1, PROMPT, theme.input);
        textarea.render(
            Rect::new(text_x, region_top + 1, text_width, text_rows),
            buf,
        );
    }
    // Footer rows fill the bottom of the screen.
    let footer_top = height.saturating_sub(footer_rows);
    for (row, line) in (footer_top..height).zip(footer) {
        buf.set_line(0, row, line, width);
    }
    Rect::new(0, 0, width, region_top)
}

/// Paint last so the message list covers transcript cells without changing wrapping.
fn render_messages(
    buf: &mut Buffer,
    transcript: Rect,
    overlay: &MessageOverlay<'_>,
    theme: &Theme,
) -> MessageGeometry {
    let max_width = transcript.width / 2;
    let max_height = transcript.height / 2;
    // Keep a border and one text row even before the session has messages.
    if max_width < 5 || max_height < 3 {
        return MessageGeometry::default();
    }
    let content_width = overlay
        .entries
        .iter()
        .map(|entry| Line::raw(entry.as_str()).width())
        .max()
        .unwrap_or("(empty)".len());
    let width = content_width.saturating_add(2).min(max_width as usize) as u16;
    let rows = overlay.entries.len().max(1).min((max_height - 2) as usize);
    let max_scroll = overlay.entries.len().saturating_sub(rows);
    let scroll = overlay.scroll.min(max_scroll);
    let area = Rect::new(
        transcript.right() - width,
        transcript.y,
        width,
        rows as u16 + 2,
    );
    Clear.render(area, buf);
    buf.set_style(area, theme.status);
    Border::default()
        .borders(Borders::ALL)
        .border_style(theme.status)
        .render(area, buf);
    let entries = &overlay.entries[scroll..overlay.entries.len().min(scroll + rows)];
    for row in 0..rows {
        let entry = entries.get(row).map(String::as_str).unwrap_or("(empty)");
        let y = area.y + 1 + row as u16;
        buf.set_style(Rect::new(area.x + 1, y, width - 2, 1), theme.status);
        buf.set_string(
            area.x + 1,
            y,
            clip_line(entry, (width - 2) as usize),
            theme.status,
        );
    }
    MessageGeometry {
        area,
        scroll,
        max_scroll,
    }
}

/// Preserve grapheme boundaries and reserve one terminal cell for the ellipsis.
fn clip_line(text: &str, width: usize) -> String {
    // Stored messages can contain controls; never let them change terminal row layout.
    let text: String = text
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect();
    if Line::raw(text.as_str()).width() <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    let mut short = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = Line::raw(grapheme).width();
        if used + cells > width - 1 {
            break;
        }
        short.push_str(grapheme);
        used += cells;
    }
    short.push('…');
    short
}

fn restore(out: &mut impl Write) -> io::Result<()> {
    // Ending a synchronized update that isn't open, or leaving an alternate screen
    // that wasn't entered, is harmless; this covers exits (including panics) that
    // happen mid-frame or mid-setup.
    queue!(
        out,
        EndSynchronizedUpdate,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    )?;
    out.flush()?;
    terminal::disable_raw_mode()
}

/// Restore the terminal before the default panic message so it is readable.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore(&mut io::stdout());
        default(info);
    }));
}

/// Number of screen rows the textarea's content occupies at `width`, using the
/// textarea's own wrapping. It exposes no row count, so render a scratch copy to
/// build its screen map and read the screen row of the end of the text.
fn wrapped_rows(textarea: &TextArea, width: u16) -> u16 {
    let mut probe = textarea.clone();
    probe.move_cursor(CursorMove::Bottom);
    probe.move_cursor(CursorMove::End);
    // The screen map covers all text regardless of the rendered height.
    let area = Rect::new(0, 0, width, 1);
    let mut scratch = Buffer::empty(area);
    (&probe).render(area, &mut scratch);
    (probe.screen_cursor().row + 1) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load the example's colors for buffer assertions.
    fn theme() -> Theme {
        let config: toml::Value = toml::from_str(include_str!("../config.toml.example")).unwrap();
        config["theme"].clone().try_into().unwrap()
    }

    /// New positions appear at the bottom, while scrolling keeps older rows available.
    #[test]
    fn overlay_scrolls_and_clips_to_existing_limits() {
        let theme = theme();
        let area = Rect::new(0, 0, 20, 10);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(10, 1, "abcdefghij", theme.status);
        let entries: Vec<_> = (0..8)
            .map(|index| format!("{index} long preview"))
            .collect();
        let mut overlay = MessageOverlay {
            entries: &entries,
            scroll: usize::MAX,
        };
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert_eq!(geometry.area, Rect::new(10, 0, 10, 5));
        assert_eq!(geometry.scroll, 5);
        assert_eq!(geometry.max_scroll, 5);
        assert_eq!(buffer[(11, 1)].symbol(), "5");
        assert_eq!(buffer[(18, 1)].symbol(), "…");
        assert_eq!(buffer[(19, 1)].symbol(), "│");
        overlay.scroll = 1;
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert_eq!(geometry.scroll, 1);
        assert_eq!(buffer[(11, 1)].symbol(), "1");
    }

    /// The permanent box shows an empty state when space permits and yields on tiny screens.
    #[test]
    fn empty_and_tiny_overlays() {
        let theme = theme();
        let overlay = MessageOverlay {
            entries: &[],
            scroll: 0,
        };
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert!(geometry.contains(79, 2));
        assert_eq!(buffer[(72, 1)].symbol(), "(");
        for area in [
            Rect::default(),
            Rect::new(0, 0, 9, 20),
            Rect::new(0, 0, 80, 5),
        ] {
            let mut buffer = Buffer::empty(area);
            assert!(!render_messages(&mut buffer, area, &overlay, &theme).contains(0, 0));
        }
    }

    /// Cell clipping preserves graphemes and never emits terminal controls.
    #[test]
    fn clipped_lines_preserve_graphemes_and_cell_width() {
        assert_eq!(clip_line("界界界", 4), "界…");
        assert_eq!(clip_line("e\u{301}abc", 3), "e\u{301}a…");
        assert_eq!(clip_line("abc", 1), "…");
        assert_eq!(clip_line("abc", 0), "");
        assert_eq!(clip_line("界", 2), "界");
        assert_eq!(clip_line("a\tb", 3), "a�b");
    }

    /// Input glyphs, its prompt, and the separator use explicit colors without dimming.
    #[test]
    fn input_and_chrome_use_theme() {
        let config: toml::Value = toml::from_str(include_str!("../config.toml.example")).unwrap();
        let theme: Theme = config["theme"].clone().try_into().unwrap();
        let mut textarea = crate::input::new_textarea(&theme);
        textarea.insert_str("abc");
        let area = Rect::new(0, 0, 40, 10);
        let mut buffer = Buffer::empty(area);
        render(
            &mut buffer,
            area,
            &[],
            &mut View::default(),
            &textarea,
            &[Line::styled("status", theme.status)],
            &theme,
        );
        assert_eq!(buffer[(0, 8)].fg, theme.input.fg.unwrap());
        assert_eq!(buffer[(2, 8)].fg, theme.input.fg.unwrap());
        assert_eq!(buffer[(0, 7)].fg, theme.status.fg.unwrap());
        assert_eq!(buffer[(0, 9)].fg, theme.status.fg.unwrap());
        assert!(
            !buffer[(2, 8)]
                .modifier
                .contains(ratatui::style::Modifier::DIM)
        );
    }
}
