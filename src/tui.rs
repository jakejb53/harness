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
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block as Border, Borders, Clear, Widget};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::Theme;
use crate::transcript::{Block, View};

/// Prompt marker at the start of the input box.
const PROMPT: &str = "> ";

/// The context overlay's title. The header is this followed by the name of each agent
/// on the stack in brackets (`context_header`), and the title itself is never shortened.
const CONTEXT_TITLE: &str = "[Context]";

/// Top-border cells beside the header: the two corners and the `[-]`/`[+]` toggle.
const HEADER_CHROME: usize = 5;

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
    pub entries: &'a [MessageEntry],
    pub scroll: usize,
    pub collapsed: bool,
    /// The names of the agents on the stack below the one on screen, root first. The
    /// header names them before it: `[Context][<root>]…[<name>]`.
    pub ancestors: &'a [&'a str],
    /// The agent on screen, last in the header.
    pub name: &'a str,
}

/// Context rows carry semantic segments so labels and counts can be styled independently.
pub struct MessageEntry {
    pub turn_index: usize,
    pub hidden: bool,
    pub prefix: String,
    pub label: String,
    pub turn_label: bool,
    pub tokens: String,
    pub payload: String,
}

/// Mouse routing and scroll bounds belong to the displayed frame.
#[derive(Default)]
struct MessageGeometry {
    area: Rect,
    toggle: Rect,
    turn_rows: Vec<(u16, usize)>,
    scroll: usize,
    max_scroll: usize,
}

impl MessageGeometry {
    /// Borders absorb mouse events too, preventing selection of covered transcript text.
    fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains((x, y).into())
    }

    /// Only the displayed border control or collapsed tab changes visibility.
    fn toggle_at(&self, x: u16, y: u16) -> bool {
        self.toggle.contains((x, y).into())
    }

    /// Every interior cell on a rendered entry row controls that entry's turn.
    fn turn_at(&self, x: u16, y: u16) -> Option<usize> {
        let interior = Rect::new(
            self.area.x.saturating_add(1),
            self.area.y.saturating_add(1),
            self.area.width.saturating_sub(2),
            self.area.height.saturating_sub(2),
        );
        interior
            .contains((x, y).into())
            .then(|| {
                self.turn_rows
                    .iter()
                    .find_map(|(row, turn)| (*row == y).then_some(*turn))
            })
            .flatten()
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

    /// Resolve the collapse control from the last frame, not a newly sized overlay.
    pub fn message_toggle_at(&self, x: u16, y: u16) -> bool {
        self.messages.toggle_at(x, y)
    }

    /// Resolve an editor row to its owning turn using the last rendered frame.
    pub fn message_turn_at(&self, x: u16, y: u16) -> Option<usize> {
        self.messages.turn_at(x, y)
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
    let max_width = (u32::from(transcript.width) * 9 / 16) as u16;
    let max_height = (u32::from(transcript.height) * 9 / 16) as u16;
    let min_height = if overlay.collapsed { 1 } else { 3 };
    // Controls in the names are replaced up front (`clip_line` at unlimited width), so
    // none reach the terminal and the widths measured for them are the widths drawn.
    let ancestors: Vec<String> = overlay
        .ancestors
        .iter()
        .map(|name| clip_line(name, usize::MAX))
        .collect();
    let name = clip_line(overlay.name, usize::MAX);
    // Hidden unless the cap fits the shortest header, `[Context][…]`, with the corners
    // and toggle, whatever the stack's depth.
    let min_width = Line::raw(CONTEXT_TITLE).width() + Line::raw("[…]").width() + HEADER_CHROME;
    if usize::from(max_width) < min_width || max_height < min_height {
        return MessageGeometry::default();
    }
    let content_width = overlay
        .entries
        .iter()
        .map(|entry| {
            Line::raw(entry.prefix.as_str())
                .width()
                .saturating_add(Line::raw(entry.label.as_str()).width())
                .saturating_add(3)
                .saturating_add(Line::raw(entry.tokens.as_str()).width())
                .saturating_add(usize::from(!entry.payload.is_empty()))
                .saturating_add(Line::raw(entry.payload.as_str()).width())
        })
        .max()
        .unwrap_or("(empty)".len());
    // At least wide enough for the full header, naming every agent on the stack, with
    // the corners and toggle, in both forms. Only when the cap binds does the header
    // collapse ancestors or shorten the name (`context_header`).
    let header_width = Line::raw(context_header(&ancestors, &name, usize::MAX).as_str()).width();
    let width = content_width
        .saturating_add(2)
        .max(header_width.saturating_add(HEADER_CHROME))
        .min(max_width as usize) as u16;
    if overlay.collapsed {
        let area = Rect::new(transcript.right() - width, transcript.y, width, 1);
        Clear.render(area, buf);
        Border::default()
            .borders(Borders::TOP)
            .border_style(theme.status)
            .render(area, buf);
        let toggle = render_context_header(buf, area, true, &ancestors, &name, theme);
        return MessageGeometry {
            area,
            toggle,
            turn_rows: Vec::new(),
            scroll: overlay.scroll,
            max_scroll: 0,
        };
    }
    // Keep a border and one text row even before the session has messages.
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
    let toggle = render_context_header(buf, area, false, &ancestors, &name, theme);
    let entries = &overlay.entries[scroll..overlay.entries.len().min(scroll + rows)];
    let mut turn_rows = Vec::with_capacity(entries.len());
    for row in 0..rows {
        let y = area.y + 1 + row as u16;
        let x = area.x + 1;
        let available = (width - 2) as usize;
        let Some(entry) = entries.get(row) else {
            buf.set_style(Rect::new(x, y, width - 2, 1), theme.status);
            buf.set_string(x, y, "(empty)", theme.status);
            continue;
        };
        turn_rows.push((y, entry.turn_index));
        let row_style = if entry.hidden {
            half_brightness(theme.status)
        } else {
            theme.status
        };
        buf.set_style(Rect::new(x, y, width - 2, 1), row_style);
        let mut used = 0;
        let mut label_style = if entry.turn_label {
            theme.context_turn
        } else {
            theme.context_role
        };
        let mut token_style = if entry.turn_label {
            theme.context_tokens
        } else {
            theme.context_message_tokens
        };
        if entry.hidden {
            label_style = half_brightness(label_style);
            token_style = half_brightness(token_style);
        }
        for (text, style) in [
            (entry.prefix.as_str(), row_style),
            (entry.label.as_str(), label_style),
            (" · ", row_style),
            (entry.tokens.as_str(), token_style),
            (if entry.payload.is_empty() { "" } else { " " }, row_style),
            (entry.payload.as_str(), row_style),
        ] {
            if text.is_empty() || used >= available {
                continue;
            }
            let clipped = clip_line(text, available - used);
            buf.set_string(x + used as u16, y, &clipped, style);
            used += Line::raw(clipped.as_str()).width();
            if clipped != text {
                break;
            }
        }
    }
    MessageGeometry {
        area,
        toggle,
        turn_rows,
        scroll,
        max_scroll,
    }
}

/// Reduce configured RGB foreground brightness exactly, independent of terminal dim support.
fn half_brightness(mut style: Style) -> Style {
    if let Some(Color::Rgb(red, green, blue)) = style.fg {
        style.fg = Some(Color::Rgb(red / 2, green / 2, blue / 2));
    }
    style
}

/// Draw the centered persistent header naming the agents on the stack, fitted to the
/// cells between the left corner and the toggle (`context_header`), and the visibility
/// control over the top border.
fn render_context_header(
    buf: &mut Buffer,
    area: Rect,
    collapsed: bool,
    ancestors: &[String],
    name: &str,
    theme: &Theme,
) -> Rect {
    let toggle = Rect::new(area.right() - 4, area.y, 3, 1);
    let control = if collapsed { "[+]" } else { "[-]" };
    buf.set_string(toggle.x, toggle.y, control, theme.status);
    let title_space = toggle.x.saturating_sub(area.x + 1);
    let header = context_header(ancestors, name, usize::from(title_space));
    let header_width = Line::raw(header.as_str()).width() as u16;
    let title_x = area.x + 1 + title_space.saturating_sub(header_width) / 2;
    buf.set_string(title_x, area.y, &header, theme.assistant);
    toggle
}

/// The header naming the agents on the stack, `[Context][<root>]…[<name>]`, fitted to
/// `space` cells; `ancestors` (root first) and `name` (the agent on screen) arrive with
/// their controls replaced. When the full header does not fit, the fewest oldest
/// ancestors that make it fit collapse into one `[…]` after the title, so the name stays
/// visible; with every ancestor collapsed, a name still too long shortens with
/// `clip_line`'s ellipsis. Too narrow even for `[Context][…][…]`, the one `[…]` stands
/// for the name as well, so the shortest header is `[Context][…]` at any depth, the
/// narrowest `render_messages` draws.
fn context_header(ancestors: &[String], name: &str, space: usize) -> String {
    // Collapse the oldest `collapsed` ancestors, none at first, into one `[…]`.
    for collapsed in 0..=ancestors.len() {
        let marker = if collapsed == 0 { "" } else { "[…]" };
        let kept: String = ancestors[collapsed..]
            .iter()
            .map(|ancestor| format!("[{ancestor}]"))
            .collect();
        let header = format!("{CONTEXT_TITLE}{marker}{kept}[{name}]");
        if Line::raw(header.as_str()).width() <= space {
            return header;
        }
    }
    // Every ancestor is collapsed and the name still does not fit: it shortens to the
    // cells left after the title, the marker, and its own brackets, while at least its
    // ellipsis fits; below that the header is `[Context][…]`.
    let marker = if ancestors.is_empty() { "" } else { "[…]" };
    let fixed = Line::raw(CONTEXT_TITLE).width() + Line::raw(marker).width() + 2;
    if space > fixed {
        return format!(
            "{CONTEXT_TITLE}{marker}[{}]",
            clip_line(name, space - fixed)
        );
    }
    format!("{CONTEXT_TITLE}[…]")
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
        let area = Rect::new(0, 0, 40, 16);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(25, 1, "abcdefghijklmno", theme.status);
        let entries: Vec<_> = (0..8)
            .map(|turn_index| MessageEntry {
                turn_index,
                hidden: false,
                prefix: String::new(),
                label: "user".into(),
                turn_label: false,
                tokens: "12t".into(),
                payload: "long preview".into(),
            })
            .collect();
        let mut overlay = MessageOverlay {
            entries: &entries,
            scroll: usize::MAX,
            collapsed: false,
            ancestors: &[],
            name: "test",
        };
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert_eq!(geometry.area, Rect::new(18, 0, 22, 9));
        assert_eq!(geometry.scroll, 1);
        assert_eq!(geometry.max_scroll, 1);
        assert!(geometry.toggle_at(36, 0));
        assert!(!geometry.toggle_at(35, 0));
        assert_eq!(geometry.turn_at(19, 1), Some(1));
        assert_eq!(geometry.turn_at(38, 7), Some(7));
        assert_eq!(buffer[(19, 1)].symbol(), "u");
        assert_eq!(buffer[(19, 1)].fg, theme.context_role.fg.unwrap());
        assert_eq!(buffer[(26, 1)].symbol(), "1");
        assert_eq!(buffer[(26, 1)].fg, theme.context_message_tokens.fg.unwrap());
        assert_eq!(buffer[(38, 1)].symbol(), "…");
        assert_eq!(buffer[(38, 1)].fg, theme.status.fg.unwrap());
        assert_eq!(buffer[(39, 1)].symbol(), "│");
        overlay.scroll = 0;
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert_eq!(geometry.scroll, 0);
        assert_eq!(geometry.turn_at(19, 1), Some(0));
        assert_eq!(buffer[(19, 1)].symbol(), "u");
    }

    /// Turn labels and token values use their dedicated configured colors.
    #[test]
    fn context_header_colors() {
        let theme = theme();
        let entries = [MessageEntry {
            turn_index: 2,
            hidden: false,
            prefix: String::new(),
            label: "Turn 2".into(),
            turn_label: true,
            tokens: "~123 tokens".into(),
            payload: String::new(),
        }];
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_messages(
            &mut buffer,
            area,
            &MessageOverlay {
                entries: &entries,
                scroll: 0,
                collapsed: false,
                ancestors: &[],
                name: "test",
            },
            &theme,
        );
        let x = geometry.area.x + 1;
        assert_eq!(buffer[(x, 1)].fg, theme.context_turn.fg.unwrap());
        assert_eq!(buffer[(x + 9, 1)].fg, theme.context_tokens.fg.unwrap());
        // Centered: `[Context][test]` is 15 cells, after the left corner.
        let title_x = geometry.area.x + 1 + (geometry.toggle.x - geometry.area.x - 16) / 2;
        assert_eq!(buffer[(title_x, 0)].symbol(), "[");
        assert_eq!(buffer[(title_x, 0)].fg, theme.assistant.fg.unwrap());
    }

    /// Hidden rows halve every configured text color and every row resolves to its turn.
    #[test]
    fn hidden_turn_rows_are_dimmed_and_clickable() {
        let theme = theme();
        let entries = [
            MessageEntry {
                turn_index: 4,
                hidden: true,
                prefix: String::new(),
                label: "Turn 4".into(),
                turn_label: true,
                tokens: "20 tokens".into(),
                payload: String::new(),
            },
            MessageEntry {
                turn_index: 4,
                hidden: true,
                prefix: String::new(),
                label: "user".into(),
                turn_label: false,
                tokens: "12t".into(),
                payload: "preview".into(),
            },
        ];
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_messages(
            &mut buffer,
            area,
            &MessageOverlay {
                entries: &entries,
                scroll: 0,
                collapsed: false,
                ancestors: &[],
                name: "test",
            },
            &theme,
        );
        let x = geometry.area.x + 1;
        assert_eq!(
            buffer[(x, 1)].fg,
            half_brightness(theme.context_turn).fg.unwrap()
        );
        assert_eq!(
            buffer[(x, 2)].fg,
            half_brightness(theme.context_role).fg.unwrap()
        );
        assert_eq!(
            buffer[(x + 7, 2)].fg,
            half_brightness(theme.context_message_tokens).fg.unwrap()
        );
        assert_eq!(
            buffer[(x + 11, 2)].fg,
            half_brightness(theme.status).fg.unwrap()
        );
        assert_eq!(geometry.turn_at(x, 1), Some(4));
        assert_eq!(geometry.turn_at(geometry.area.right() - 2, 2), Some(4));
        assert_eq!(geometry.turn_at(x, 0), None);
    }

    /// The permanent box shows an empty state when space permits and yields on tiny screens.
    #[test]
    fn empty_and_tiny_overlays() {
        let theme = theme();
        let overlay = MessageOverlay {
            entries: &[],
            scroll: 0,
            collapsed: false,
            ancestors: &[],
            name: "test",
        };
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert!(geometry.contains(79, 2));
        // Sized by the header: `[Context][test]` plus the corners and toggle is 20 cells.
        assert_eq!(buffer[(61, 1)].symbol(), "(");
        for area in [
            Rect::default(),
            Rect::new(0, 0, 9, 20),
            Rect::new(0, 0, 80, 5),
        ] {
            let mut buffer = Buffer::empty(area);
            assert!(!render_messages(&mut buffer, area, &overlay, &theme).contains(0, 0));
        }
    }

    /// The collapsed header retains its width, title, control, and prior scroll position.
    #[test]
    fn collapsed_overlay_is_clickable_tab() {
        let theme = theme();
        let entries: Vec<_> = (0..8)
            .map(|turn_index| MessageEntry {
                turn_index,
                hidden: false,
                prefix: String::new(),
                label: "user".into(),
                turn_label: false,
                tokens: "12t".into(),
                payload: "preview".into(),
            })
            .collect();
        let overlay = MessageOverlay {
            entries: &entries,
            scroll: 2,
            collapsed: true,
            ancestors: &[],
            name: "test",
        };
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_messages(&mut buffer, area, &overlay, &theme);
        assert_eq!(geometry.area, Rect::new(60, 0, 20, 1));
        assert_eq!(geometry.scroll, 2);
        assert!(!geometry.toggle_at(60, 0));
        assert!(geometry.toggle_at(76, 0));
        assert!(geometry.toggle_at(78, 0));
        assert!(!geometry.toggle_at(79, 0));
        assert!(!geometry.contains(60, 1));
        // `[Context][test]` exactly fills the 15 cells between the corner and toggle.
        assert_eq!(buffer[(61, 0)].symbol(), "[");
        assert_eq!(buffer[(61, 0)].fg, theme.assistant.fg.unwrap());
        assert_eq!(buffer[(76, 0)].symbol(), "[");
        assert_eq!(buffer[(78, 0)].symbol(), "]");
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
