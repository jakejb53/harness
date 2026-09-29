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
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block as Border, Borders, Clear, Widget};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_segmentation::UnicodeSegmentation;

use crate::config::Theme;
use crate::session::Injection;
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
    injections: InjectionGeometry,
}

/// Each entry identifies one persisted call, even when paths are identical.
pub struct InjectionOverlay<'a> {
    pub entries: &'a [Injection],
    pub scroll: usize,
    pub selected: Option<&'a str>,
}

/// Hit targets belong to the displayed frame, not a newly changed session list.
#[derive(Default)]
struct InjectionGeometry {
    area: Rect,
    call_ids: Vec<String>,
    max_scroll: usize,
}

impl InjectionGeometry {
    /// Borders absorb mouse events too, preventing selection of covered transcript text.
    fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains((x, y).into())
    }

    /// Resolve only a visible minus cell to its persisted injection instance.
    fn at(&self, x: u16, y: u16) -> Option<&str> {
        if !self.contains(x, y) || x != self.area.x + 1 {
            return None;
        }
        let row = y.checked_sub(self.area.y + 1)? as usize;
        self.call_ids.get(row).map(String::as_str)
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
            injections: InjectionGeometry::default(),
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
        overlay: InjectionOverlay<'_>,
    ) -> io::Result<()> {
        queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let mut injections = InjectionGeometry::default();
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
                injections = render_injections(frame.buffer_mut(), transcript, &overlay, theme);
            })
            .map(|_| ());
        if drawn.is_ok() {
            self.injections = injections;
        }
        let out = self.terminal.backend_mut();
        queue!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        drawn
    }

    /// Only the minus cell removes an injection; the rest of the overlay absorbs input.
    pub fn injection_at(&self, x: u16, y: u16) -> Option<&str> {
        self.injections.at(x, y)
    }

    /// Route all mouse events inside the displayed overlay away from the transcript.
    pub fn over_injections(&self, x: u16, y: u16) -> bool {
        self.injections.contains(x, y)
    }

    /// Bound wheel scrolling by the latest frame's available attachment rows.
    pub fn injection_max_scroll(&self) -> usize {
        self.injections.max_scroll
    }

    /// Use the current terminal width when wrapping full-path confirmation text.
    pub fn width(&self) -> io::Result<u16> {
        self.terminal.size().map(|size| size.width)
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

/// Paint last so attachments cover transcript cells without changing its wrapping.
fn render_injections(
    buf: &mut Buffer,
    transcript: Rect,
    overlay: &InjectionOverlay<'_>,
    theme: &Theme,
) -> InjectionGeometry {
    let max_width = transcript.width / 2;
    let max_height = transcript.height / 2;
    // A border, minus, separator and at least one filename cell must fit.
    if overlay.entries.is_empty() || max_width < 5 || max_height < 3 {
        return InjectionGeometry::default();
    }
    let name_width = overlay
        .entries
        .iter()
        .map(|entry| Line::raw(entry.path.as_str()).width())
        .max()
        .unwrap_or(1);
    let width = name_width.saturating_add(4).min(max_width as usize) as u16;
    let rows = overlay.entries.len().min((max_height - 2) as usize);
    let max_scroll = overlay.entries.len() - rows;
    let mut scroll = overlay.scroll.min(max_scroll);
    // A wrapped confirmation shrinks the transcript; keep its selected row visible.
    if let Some(index) = overlay
        .entries
        .iter()
        .position(|entry| overlay.selected == Some(entry.call_id.as_str()))
    {
        if index < scroll {
            scroll = index;
        } else if index >= scroll + rows {
            scroll = index + 1 - rows;
        }
    }
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
    let entries = &overlay.entries[scroll..scroll + rows];
    for (row, entry) in entries.iter().enumerate() {
        let style = if overlay.selected == Some(entry.call_id.as_str()) {
            theme.status.add_modifier(Modifier::REVERSED)
        } else {
            theme.status
        };
        let y = area.y + 1 + row as u16;
        buf.set_style(Rect::new(area.x + 1, y, width - 2, 1), style);
        buf.set_string(area.x + 1, y, "- ", style);
        buf.set_string(
            area.x + 3,
            y,
            shorten_path(&entry.path, (width - 4) as usize),
            style,
        );
    }
    InjectionGeometry {
        area,
        call_ids: entries.iter().map(|entry| entry.call_id.clone()).collect(),
        max_scroll,
    }
}

/// Preserve grapheme boundaries and reserve one terminal cell for the ellipsis.
fn shorten_path(path: &str, width: usize) -> String {
    // Filenames can contain controls; never let them change terminal row layout.
    let path: String = path
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect();
    if Line::raw(path.as_str()).width() <= width {
        return path;
    }
    if width == 0 {
        return String::new();
    }
    let mut short = String::new();
    let mut used = 0;
    for grapheme in path.graphemes(true) {
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

    fn theme() -> Theme {
        let config: toml::Value = toml::from_str(include_str!("../config.toml.example")).unwrap();
        config["theme"].clone().try_into().unwrap()
    }

    #[test]
    fn overlay_tracks_duplicate_instances_and_only_minus_hits() {
        let theme = theme();
        let area = Rect::new(0, 0, 40, 16);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(39, 15, "x", theme.status);
        let entries = vec![
            Injection {
                call_id: "first".into(),
                path: "same.rs".into(),
            },
            Injection {
                call_id: "second".into(),
                path: "same.rs".into(),
            },
        ];
        let geometry = render_injections(
            &mut buffer,
            area,
            &InjectionOverlay {
                entries: &entries,
                scroll: 0,
                selected: Some("second"),
            },
            &theme,
        );
        assert_eq!(geometry.area, Rect::new(29, 0, 11, 4));
        assert_eq!(geometry.at(30, 1), Some("first"));
        assert_eq!(geometry.at(30, 2), Some("second"));
        assert_eq!(geometry.at(31, 1), None);
        assert_eq!(geometry.at(30, 0), None);
        assert_eq!(geometry.at(30, 3), None);
        assert!(geometry.contains(39, 3));
        assert!(!geometry.contains(39, 4));
        assert_eq!(buffer[(30, 1)].symbol(), "-");
        assert!(buffer[(30, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(30, 1)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(39, 15)].symbol(), "x");
    }

    #[test]
    fn overlay_clamps_scrolling_and_clears_covered_transcript() {
        let theme = theme();
        let area = Rect::new(0, 0, 20, 10);
        let mut buffer = Buffer::empty(area);
        buffer.set_string(10, 1, "abcdefghij", theme.status);
        let entries: Vec<_> = (0..8)
            .map(|index| Injection {
                call_id: index.to_string(),
                path: "a/very/long/path".into(),
            })
            .collect();
        let geometry = render_injections(
            &mut buffer,
            area,
            &InjectionOverlay {
                entries: &entries,
                scroll: usize::MAX,
                selected: None,
            },
            &theme,
        );
        assert_eq!(geometry.area, Rect::new(10, 0, 10, 5));
        assert_eq!(geometry.max_scroll, 5);
        assert_eq!(geometry.call_ids, ["5", "6", "7"]);
        assert_eq!(buffer[(12, 1)].symbol(), " ");
        assert_eq!(buffer[(18, 1)].symbol(), "…");
        assert_eq!(geometry.at(11, 1), Some("5"));
        // A confirmation footer can shrink the box to one row after clicking.
        for (scroll, selected) in [(0, "7"), (usize::MAX, "0")] {
            let geometry = render_injections(
                &mut buffer,
                Rect::new(0, 0, 20, 6),
                &InjectionOverlay {
                    entries: &entries,
                    scroll,
                    selected: Some(selected),
                },
                &theme,
            );
            assert_eq!(geometry.call_ids, [selected]);
            assert_eq!(geometry.at(11, 1), Some(selected));
            assert!(buffer[(11, 1)].modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn empty_or_tiny_overlay_has_no_hit_targets() {
        let theme = theme();
        let entries = [Injection {
            call_id: "one".into(),
            path: "file".into(),
        }];
        for area in [
            Rect::default(),
            Rect::new(0, 0, 9, 20),
            Rect::new(0, 0, 80, 5),
        ] {
            let mut buffer = Buffer::empty(area);
            let geometry = render_injections(
                &mut buffer,
                area,
                &InjectionOverlay {
                    entries: &entries,
                    scroll: 0,
                    selected: None,
                },
                &theme,
            );
            assert!(geometry.call_ids.is_empty());
            assert!(!geometry.contains(0, 0));
        }
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);
        let geometry = render_injections(
            &mut buffer,
            area,
            &InjectionOverlay {
                entries: &[],
                scroll: 0,
                selected: None,
            },
            &theme,
        );
        assert!(geometry.call_ids.is_empty());
        assert!(!geometry.contains(0, 0));
    }

    #[test]
    fn shortened_paths_preserve_graphemes_and_cell_width() {
        assert_eq!(shorten_path("界界界", 4), "界…");
        assert_eq!(shorten_path("e\u{301}abc", 3), "e\u{301}a…");
        assert_eq!(shorten_path("abc", 1), "…");
        assert_eq!(shorten_path("abc", 0), "");
        assert_eq!(shorten_path("界", 2), "界");
        assert_eq!(shorten_path("a\tb", 3), "a�b");
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
