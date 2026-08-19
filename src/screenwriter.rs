// Frame composition: draws the visible rows plus a status bar. Adapted
// from jless's src/screenwriter.rs — same responsibility (line numbers,
// status bar, frame redraw), but the line-truncation story is simplified:
// jless uses its ~1125-line truncatedstrview.rs for horizontal scrolling
// of the focused line and mid-string ellipsis elsewhere; this just
// hard-truncates to the terminal width using unicode-width for
// correctness on wide characters. Deferring the fuller truncation/scroll
// behavior was a deliberate scope cut — see lineprinter.rs's module doc
// comment.

use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::flatxml::{Index, OptionIndex};
use crate::highlighting;
use crate::lineprinter;
use crate::search::SearchState;
use crate::terminal::{AnsiTerminal, Style, Terminal};
use crate::types::TTYDimensions;
use crate::viewer::Viewer;

pub struct ScreenWriter<W: IoWrite> {
    pub stdout: W,
    pub terminal: AnsiTerminal,
    pub dimensions: TTYDimensions,
    pub show_line_numbers: bool,
    pub show_relative_line_numbers: bool,
}

fn truncate_to_width(s: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut width = 0usize;
    for (byte_idx, ch) in s.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max_width {
            return &s[..byte_idx];
        }
        width += w;
    }
    s
}

impl<W: IoWrite> ScreenWriter<W> {
    pub fn new(stdout: W, dimensions: TTYDimensions) -> Self {
        ScreenWriter {
            stdout,
            terminal: AnsiTerminal::new(),
            dimensions,
            show_line_numbers: true,
            show_relative_line_numbers: false,
        }
    }

    pub fn print(
        &mut self,
        viewer: &Viewer,
        filename: &str,
        dirty: bool,
        search: &SearchState,
        message: &Option<String>,
    ) {
        let doc = &viewer.doc;

        self.terminal.output.clear();
        let _ = self.terminal.clear_screen();

        let content_height = self.dimensions.without_status_bar().height;

        // Gutter width: big enough for the largest line number that could
        // ever appear, plus one column of padding. Sized dynamically
        // rather than fixed so small documents don't waste five columns
        // on line numbers nobody needs.
        //
        // BUG FIX (found by review, confirmed independently twice): this
        // used to size off `content_height` (the terminal's visible row
        // count, typically a couple dozen) instead of `doc.flat.len()`
        // (the document's actual row count, which — per
        // docs/ARCHITECTURE.md §8 — is exactly the number that scales to
        // millions on a 100-200MB file). Every row's own number is the
        // *absolute* row index (`write_gutter` always shows it for the
        // focused row, and for every row when relative numbers are off),
        // so on any document taller than one screen the printed number
        // silently overflowed the reserved width — `format!("{n:>width$}
        // ")` treats `width` as a minimum, not a cap — throwing off
        // `content_width` and misaligning every row with a different
        // digit count from its neighbors.
        let gutter_width = if self.show_line_numbers {
            let max_n = doc.flat.len();
            format!("{max_n}").len() + 1
        } else {
            0
        };
        let content_width = (self.dimensions.width as usize).saturating_sub(gutter_width);

        let mut row = viewer.top_row;
        for screen_row in 0..content_height {
            let _ = self.terminal.position_cursor(1, screen_row + 1);

            if self.show_line_numbers {
                self.write_gutter(viewer, row, screen_row, gutter_width);
            }

            let depth = doc.flat[row].depth as usize;
            let indent = "  ".repeat(depth);
            let content = lineprinter::render_row(doc, row, viewer.mode);
            let mut style = highlighting::style_for(&doc.flat[row].value);

            let focused = row == viewer.focused_row;
            let is_match = search.current_match == Some(row);
            let mut line = String::with_capacity(indent.len() + content.len());
            let _ = write!(line, "{indent}{content}");
            let line = truncate_to_width(&line, content_width);

            style.inverted = focused;
            if is_match && !focused {
                style.bg = highlighting::SEARCH_MATCH_BG;
            }
            let _ = self.terminal.set_style(&style);
            let _ = self.terminal.write_str(line);
            let _ = self.terminal.reset_style();

            match viewer.next_display_row(row) {
                OptionIndex::Index(next) => row = next,
                OptionIndex::Nil => {
                    for blank_row in (screen_row + 1)..content_height {
                        let _ = self.terminal.position_cursor(1, blank_row + 1);
                    }
                    break;
                }
            }
        }

        self.print_status_bar_into_buffer(viewer, filename, dirty, search, message);

        let _ = self.terminal.flush_contents(&mut self.stdout);
    }

    fn write_gutter(&mut self, viewer: &Viewer, row: Index, screen_row: u16, gutter_width: usize) {
        let n = if self.show_relative_line_numbers && row != viewer.focused_row {
            let focused_screen_row = viewer.index_of_focused_row_on_screen();
            (screen_row as i64 - focused_screen_row as i64).unsigned_abs() as usize
        } else {
            (row + 1) as usize
        };
        let text = format!("{n:>width$} ", width = gutter_width - 1);
        let mut style = Style::default();
        style.fg = highlighting::LINE_NUMBER;
        style.dimmed = row != viewer.focused_row;
        let _ = self.terminal.set_style(&style);
        let _ = self.terminal.write_str(&text);
        let _ = self.terminal.reset_style();
    }

    fn print_status_bar_into_buffer(
        &mut self,
        viewer: &Viewer,
        filename: &str,
        dirty: bool,
        search: &SearchState,
        message: &Option<String>,
    ) {
        let _ = self.terminal.position_cursor(1, self.dimensions.height);
        let _ = self.terminal.clear_line();

        let status = if let Some(msg) = message {
            msg.clone()
        } else {
            let total = viewer.doc.flat.len();
            let dirty_marker = if dirty { " [+]" } else { "" };
            let search_marker = if search.has_term() {
                format!("  {}{}", search.direction.prompt_char(), search.term)
            } else {
                String::new()
            };
            format!(
                "{filename}{dirty_marker}  [{}, row {}/{total}]{search_marker}  :help",
                viewer.mode.label(),
                viewer.focused_row + 1
            )
        };

        let status = truncate_to_width(&status, self.dimensions.width as usize);
        let mut style = Style::default();
        style.inverted = true;
        let _ = self.terminal.set_style(&style);
        let _ = self.terminal.write_str(status);
        let pad = (self.dimensions.width as usize).saturating_sub(status.width());
        for _ in 0..pad {
            let _ = self.terminal.write_str(" ");
        }
        let _ = self.terminal.reset_style();
    }

    /// Draws a live `prompt` + `buffer` on the status line, with the
    /// cursor left positioned right after it — used while the user is
    /// typing a `:`/`/`/`?` command (app.rs calls this once per
    /// keystroke, so it needs to be cheap and not touch the rest of the
    /// frame).
    pub fn print_input_line(&mut self, prompt: &str, buffer: &str) {
        self.terminal.output.clear();
        let _ = self.terminal.position_cursor(1, self.dimensions.height);
        let _ = self.terminal.clear_line();
        let text = format!("{prompt}{buffer}");
        let text = truncate_to_width(&text, self.dimensions.width as usize);
        let _ = self.terminal.write_str(text);
        let _ = self.terminal.flush_contents(&mut self.stdout);
    }

    /// Full-screen keyboard-reference overlay for `:h`/`:help` (app.rs's
    /// `HELP_TEXT`) — replaces the document view for one frame. It's a
    /// small pager in its own right (app.rs's `handle_help_key` scrolls
    /// it with j/k/arrows/Ctrl-d/Ctrl-u/g/G, closes it with q/Esc): an
    /// earlier version had no `scroll` parameter and just truncated
    /// anything past the first screen, which made most of the reference
    /// unreachable on a normal-sized terminal — found immediately by a
    /// user pressing the down arrow and having the whole screen close
    /// instead of scrolling.
    pub fn print_help(&mut self, lines: &[&str], scroll: usize) {
        self.terminal.output.clear();
        let _ = self.terminal.clear_screen();

        let content_height = self.dimensions.without_status_bar().height;
        let width = self.dimensions.width as usize;

        for screen_row in 0..content_height {
            let _ = self.terminal.position_cursor(1, screen_row + 1);
            if let Some(line) = lines.get(scroll + screen_row as usize) {
                let _ = self.terminal.write_str(truncate_to_width(line, width));
            }
        }

        let _ = self.terminal.position_cursor(1, self.dimensions.height);
        let _ = self.terminal.clear_line();
        let mut style = Style::default();
        style.inverted = true;
        let _ = self.terminal.set_style(&style);
        let last_shown = (scroll + content_height as usize).min(lines.len());
        let footer = format!(
            "lines {}-{} of {}   j/k/arrows/Ctrl-d/Ctrl-u scroll, g/G top/bottom, q/Esc close",
            (scroll + 1).min(lines.len()),
            last_shown,
            lines.len()
        );
        let footer = truncate_to_width(&footer, width);
        let _ = self.terminal.write_str(footer);
        let pad = width.saturating_sub(footer.width());
        for _ in 0..pad {
            let _ = self.terminal.write_str(" ");
        }
        let _ = self.terminal.reset_style();

        let _ = self.terminal.flush_contents(&mut self.stdout);
    }
}
