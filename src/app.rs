// Top-level event loop: reads TuiEvents, translates key sequences into
// viewer::Action (or search/yank/edit/command-mode operations), applies
// them, redraws. Adapted from jless's src/app.rs — the multi-key prefix
// handling (`dd`, `y<target>`), digit-count-prefixed movement, `:`
// command mode, and mouse handling all mirror jless's interaction model;
// the editing commands (i/r/o/O/p/P/u/Ctrl-r) are new, since jless has no
// editing at all (docs/ARCHITECTURE.md §9).

use std::io::Write as IoWrite;
use std::path::PathBuf;

use termion::event::{Key, MouseButton, MouseEvent};

use crate::clipboard;
use crate::edit::{self, EditHistory};
use crate::flatxml::Index;
use crate::input::TuiEvent;
use crate::path;
use crate::screenwriter::ScreenWriter;
use crate::search::{Direction, SearchState};
use crate::types::TTYDimensions;
use crate::viewer::{Action, Viewer};

enum Flow {
    Continue,
    Quit,
}

pub struct App<W: IoWrite> {
    pub viewer: Viewer,
    pub screen_writer: ScreenWriter<W>,
    pub filename: String,
    pub file_path: Option<PathBuf>,
    search: SearchState,
    edit_history: EditHistory,
    count_buffer: String,
    pending: Option<char>,
    message: Option<String>,
}

impl<W: IoWrite> App<W> {
    pub fn new(viewer: Viewer, filename: String, file_path: Option<PathBuf>, stdout: W) -> App<W> {
        let dimensions = viewer.dimensions;
        App {
            viewer,
            screen_writer: ScreenWriter::new(stdout, dimensions),
            filename,
            file_path,
            search: SearchState::default(),
            edit_history: EditHistory::default(),
            count_buffer: String::new(),
            pending: None,
            message: None,
        }
    }

    /// Sets a one-shot status message to show on the very first frame —
    /// for startup conditions the caller (main.rs) discovers before an
    /// `App` exists to route a normal in-session message through, e.g.
    /// `--focus-line` naming a line past the end of the file (found by
    /// review: this used to be silently ignored, landing at row 0 with no
    /// feedback that the requested line didn't exist).
    pub fn set_initial_message(&mut self, msg: String) {
        self.message = Some(msg);
    }

    pub fn run<I: Iterator<Item = std::io::Result<TuiEvent>>>(&mut self, mut input: I) {
        self.draw();

        while let Some(event) = input.next() {
            let event = match event {
                Ok(e) => e,
                Err(_) => continue,
            };

            // Any status message (from yank/save/undo/search/errors/...)
            // is meant to be shown for exactly one frame: the redraw right
            // after the action that set it. Clearing it here, before
            // dispatching *this* event, means a message set while
            // handling this event survives to this iteration's `draw()`
            // below, and the next event's handling starts clean.
            self.message = None;

            let flow = match event {
                TuiEvent::WinChEvent => {
                    if let Some(dims) = query_terminal_size() {
                        self.viewer.dimensions = dims;
                        self.screen_writer.dimensions = dims;
                        self.viewer
                            .perform_action(Action::ResizeViewerDimensions(dims));
                    }
                    Flow::Continue
                }
                TuiEvent::KeyEvent(key) => self.handle_key(key, &mut input),
                TuiEvent::MouseEvent(me) => self.handle_mouse(me),
                TuiEvent::Unknown(bytes) => {
                    self.message = Some(format!("unrecognized input: {bytes:?}"));
                    Flow::Continue
                }
            };

            match flow {
                Flow::Quit => break,
                Flow::Continue => self.draw(),
            }
        }
    }

    fn handle_mouse(&mut self, me: MouseEvent) -> Flow {
        match me {
            MouseEvent::Press(MouseButton::Left, _, row) => {
                let content_height = self.screen_writer.dimensions.height.saturating_sub(1);
                if row <= content_height {
                    self.viewer.perform_action(Action::Click(row - 1));
                }
            }
            MouseEvent::Press(MouseButton::WheelUp, _, _) => {
                self.viewer.perform_action(Action::ScrollUp(3));
            }
            MouseEvent::Press(MouseButton::WheelDown, _, _) => {
                self.viewer.perform_action(Action::ScrollDown(3));
            }
            _ => {}
        }
        Flow::Continue
    }

    fn take_count(&mut self) -> usize {
        let n = self.count_buffer.parse().unwrap_or(1).max(1);
        self.count_buffer.clear();
        n
    }

    fn handle_key<I: Iterator<Item = std::io::Result<TuiEvent>>>(
        &mut self,
        key: Key,
        input: &mut I,
    ) -> Flow {
        // Digit-count prefix: accumulate and wait for the actual command,
        // same as vim's `3j`. A leading '0' is itself a command (focus
        // first sibling convention, `^`-adjacent) rather than a count, to
        // match the "0 doesn't start a count" vim convention — but once
        // *any* digit has been typed, a following '0' is part of the
        // count, same as vim.
        if let Key::Char(c @ '0'..='9') = key {
            if !(c == '0' && self.count_buffer.is_empty()) {
                self.count_buffer.push(c);
                self.pending = None;
                return Flow::Continue;
            }
        }

        // Two-key prefix commands: `dd` (delete) and `y<target>` (yank).
        if let Some(prefix) = self.pending.take() {
            self.count_buffer.clear();
            return self.handle_prefixed_key(prefix, key);
        }

        match key {
            Key::Ctrl('c') => return Flow::Quit,
            Key::Char('q') => {
                if self.edit_history.is_dirty() {
                    self.message = Some(
                        "unsaved changes — :w to save, :q! to discard, or :wq to save & quit"
                            .to_string(),
                    );
                    return Flow::Continue;
                }
                return Flow::Quit;
            }

            Key::Char('j') | Key::Down => {
                let n = self.take_count();
                self.viewer.perform_action(Action::MoveDown(n));
            }
            Key::Char('k') | Key::Up => {
                let n = self.take_count();
                self.viewer.perform_action(Action::MoveUp(n));
            }
            Key::Char('h') | Key::Left => self.viewer.perform_action(Action::MoveLeft),
            Key::Char('l') | Key::Right => self.viewer.perform_action(Action::MoveRight),
            Key::Char('H') => self.viewer.perform_action(Action::FocusParent),
            Key::Char('J') => {
                let n = self.take_count();
                self.viewer.perform_action(Action::FocusNextSibling(n));
            }
            Key::Char('K') => {
                let n = self.take_count();
                self.viewer.perform_action(Action::FocusPrevSibling(n));
            }
            Key::Char('^') => self.viewer.perform_action(Action::FocusFirstSibling),
            Key::Char('$') => self.viewer.perform_action(Action::FocusLastSibling),
            Key::Char('g') | Key::Home => {
                self.count_buffer.clear();
                self.viewer.perform_action(Action::FocusTop);
            }
            Key::Char('G') | Key::End => {
                if self.count_buffer.is_empty() {
                    self.viewer.perform_action(Action::FocusBottom);
                } else {
                    let n = self.take_count();
                    let row = (n as Index - 1).min(self.viewer.doc.flat.len() as Index - 1);
                    self.viewer.perform_action(Action::JumpTo(row));
                }
            }
            Key::Char('%') => self.viewer.perform_action(Action::FocusMatchingPair),
            Key::Char(' ') | Key::Char('\n') => self.viewer.perform_action(Action::ToggleCollapsed),
            Key::Char('c') => self.viewer.perform_action(Action::CollapseNodeAndSiblings),
            Key::Char('e') => self.viewer.perform_action(Action::ExpandNodeAndSiblings),
            Key::Char('m') => self.viewer.perform_action(Action::ToggleMode),
            Key::Ctrl('d') | Key::PageDown => {
                let n = self.take_count();
                self.viewer.perform_action(Action::PageDown(n));
            }
            Key::Ctrl('u') | Key::PageUp => {
                let n = self.take_count();
                self.viewer.perform_action(Action::PageUp(n));
            }
            Key::Ctrl('e') => self.viewer.perform_action(Action::ScrollDown(1)),
            Key::Ctrl('y') => self.viewer.perform_action(Action::ScrollUp(1)),
            Key::Char('t') => self.viewer.perform_action(Action::MoveFocusedLineToTop),
            Key::Char('z') => self.viewer.perform_action(Action::MoveFocusedLineToCenter),
            Key::Char('b') => self.viewer.perform_action(Action::MoveFocusedLineToBottom),

            Key::Char('d') | Key::Char('y') => {
                self.pending = Some(if key == Key::Char('d') { 'd' } else { 'y' });
            }

            Key::Char('r') => self.rename_focused(input),
            Key::Char('i') => self.edit_content(input),
            Key::Char('o') => self.insert_sibling(input, true),
            Key::Char('O') => self.insert_sibling(input, false),
            Key::Char('p') => self.paste_sibling(true),
            Key::Char('P') => self.paste_sibling(false),
            Key::Char('u') => self.do_undo(),
            Key::Ctrl('r') => self.do_redo(),

            Key::Char('/') => self.start_search(input, Direction::Forward),
            Key::Char('?') => self.start_search(input, Direction::Reverse),
            // 'n' repeats the last search in whichever direction it was
            // originally made ('?' searches stay "backwards" on 'n');
            // 'N' repeats it in the opposite direction — vim's convention,
            // not "n=forward/N=backward" regardless of how the search
            // started.
            Key::Char('n') => self.jump_to_match(self.search.direction),
            Key::Char('N') => self.jump_to_match(self.search.direction.reversed()),

            Key::Char(':') => return self.command_mode(input),

            Key::Esc => self.count_buffer.clear(),
            _ => {}
        }

        Flow::Continue
    }

    fn handle_prefixed_key(&mut self, prefix: char, key: Key) -> Flow {
        match (prefix, key) {
            ('d', Key::Char('d')) => self.delete_focused(),
            ('y', Key::Char('y')) => self.yank(crate::lineprinter::pretty_printed_subtree),
            ('y', Key::Char('l')) => self.yank(crate::lineprinter::one_line_subtree),
            ('y', Key::Char('t')) => self.yank(crate::lineprinter::text_content),
            ('y', Key::Char('n')) => self.yank(|doc, i| doc.row_text(i).to_string()),
            ('y', Key::Char('x')) => self.yank_xpath(),
            _ => {}
        }
        Flow::Continue
    }

    // --- Read-a-line helper, shared by search and command mode -------

    /// Reads a line of raw input from the terminal, redrawing the status
    /// line on every keystroke, until Enter (returns the buffer) or Esc
    /// (returns None). This is xless's whole "command line" input
    /// mechanism — deliberately not a readline library (jless uses
    /// `rustyline`, which needs the `/dev/tty` remap dance to coexist
    /// with piped stdin; xless doesn't take that dependency for a feature
    /// this small — see input.rs's module doc comment).
    fn read_line<I: Iterator<Item = std::io::Result<TuiEvent>>>(
        &mut self,
        input: &mut I,
        prompt: &str,
        initial: &str,
    ) -> Option<String> {
        let mut buffer = initial.to_string();
        self.screen_writer.print_input_line(prompt, &buffer);

        for event in input {
            let event = match event {
                Ok(e) => e,
                Err(_) => continue,
            };
            match event {
                TuiEvent::KeyEvent(Key::Char('\n')) => return Some(buffer),
                TuiEvent::KeyEvent(Key::Esc) | TuiEvent::KeyEvent(Key::Ctrl('c')) => return None,
                TuiEvent::KeyEvent(Key::Backspace) => {
                    buffer.pop();
                }
                TuiEvent::KeyEvent(Key::Ctrl('u')) => buffer.clear(),
                TuiEvent::KeyEvent(Key::Char(c)) => buffer.push(c),
                TuiEvent::WinChEvent => {
                    if let Some(dims) = query_terminal_size() {
                        self.viewer.dimensions = dims;
                        self.screen_writer.dimensions = dims;
                    }
                }
                _ => continue,
            }
            self.screen_writer.print_input_line(prompt, &buffer);
        }
        None
    }

    // --- Search --------------------------------------------------------

    fn start_search<I: Iterator<Item = std::io::Result<TuiEvent>>>(
        &mut self,
        input: &mut I,
        direction: Direction,
    ) {
        let prompt = direction.prompt_char().to_string();
        let Some(term) = self.read_line(input, &prompt, "") else {
            return;
        };
        if term.is_empty() {
            return;
        }
        match self.search.set_term(term, direction) {
            Ok(()) => self.jump_to_match(direction),
            Err(e) => self.message = Some(e),
        }
    }

    fn jump_to_match(&mut self, direction: Direction) {
        if !self.search.has_term() {
            self.message = Some("no active search".to_string());
            return;
        }
        match self
            .search
            .find(&self.viewer.doc, self.viewer.focused_row, direction)
        {
            Some(row) => {
                self.search.current_match = Some(row);
                self.viewer.perform_action(Action::JumpTo(row));
                self.viewer.perform_action(Action::MoveFocusedLineToCenter);
            }
            None => self.message = Some(format!("no matches for /{}", self.search.term)),
        }
    }

    // --- Yank ------------------------------------------------------------

    fn yank(&mut self, render: impl Fn(&crate::document::Document, Index) -> String) {
        let text = render(&self.viewer.doc, self.viewer.focused_row);
        self.yank_text(text);
    }

    /// XPath is the one yank target that can legitimately not exist for
    /// the focused row (a DocType declaration — see `path::build_xpath`'s
    /// doc comment), so it gets its own method instead of going through
    /// the infallible `yank` helper above.
    fn yank_xpath(&mut self) {
        match path::build_xpath(&self.viewer.doc, self.viewer.focused_row) {
            Ok(text) => self.yank_text(text),
            Err(e) => self.message = Some(e),
        }
    }

    fn yank_text(&mut self, text: String) {
        let preview: String = text.chars().take(40).collect();
        match clipboard::copy(&text) {
            Ok(()) => self.message = Some(format!("yanked: {preview}")),
            Err(e) => self.message = Some(format!("yank failed ({e}); value: {preview}")),
        }
    }

    // --- Editing ---------------------------------------------------------

    fn commit_edit(&mut self, edits: Vec<(std::ops::Range<usize>, Vec<u8>)>, focus_hint: usize) {
        match self
            .edit_history
            .commit(&mut self.viewer, edits, focus_hint)
        {
            Ok(()) => {
                // A successful commit fully re-parses the document
                // (edit.rs's module doc comment), renumbering every row
                // from scratch — a stale `current_match: Some(old_index)`
                // would then paint whatever unrelated row now happens to
                // occupy that index as a search match. Cleared here (not
                // just left for the next `/`/`n`) since a rejected edit
                // leaves row numbering untouched and a still-valid match
                // shouldn't lose its highlight over nothing happening.
                self.search.current_match = None;
                self.message = Some("edited".to_string());
            }
            Err(e) => self.message = Some(format!("edit rejected: {e}")),
        }
    }

    fn rename_focused<I: Iterator<Item = std::io::Result<TuiEvent>>>(&mut self, input: &mut I) {
        let doc = &self.viewer.doc;
        let index = edit::resolve_container_row(doc, self.viewer.focused_row);
        let is_element = matches!(
            doc.flat[index].value,
            crate::flatxml::Value::OpenElement { .. } | crate::flatxml::Value::EmptyElement { .. }
        );
        if !is_element {
            self.message = Some("nothing to rename here".to_string());
            return;
        }

        let current = doc.row_text(index).to_string();
        let Some(new_name) = self.read_line(input, "rename to: ", &current) else {
            return;
        };
        if new_name.is_empty() || new_name == current {
            return;
        }

        let doc = &self.viewer.doc;
        let mut edits = vec![(
            doc.flat[index].range.start as usize..doc.flat[index].range.end as usize,
            new_name.clone().into_bytes(),
        )];
        if let crate::flatxml::Value::OpenElement { close_index, .. } = doc.flat[index].value {
            let close = &doc.flat[close_index];
            edits.push((
                close.range.start as usize..close.range.end as usize,
                new_name.into_bytes(),
            ));
        }
        let focus_hint = doc.flat[index].range.start as usize;
        self.commit_edit(edits, focus_hint);
    }

    fn edit_content<I: Iterator<Item = std::io::Result<TuiEvent>>>(&mut self, input: &mut I) {
        use crate::flatxml::Value;

        // Gather everything we need from `self.viewer.doc` as owned values
        // *before* calling `self.read_line` (which needs `&mut self` and
        // so can't coexist with a borrow of `self.viewer.doc`).
        enum Kind {
            Text,
            Raw,
            Attrs,
        }
        let doc = &self.viewer.doc;
        let index = edit::resolve_container_row(doc, self.viewer.focused_row);
        let (kind, range, current) = match &doc.flat[index].value {
            Value::Text => {
                let r = doc.flat[index].range.start as usize..doc.flat[index].range.end as usize;
                (Kind::Text, r.clone(), doc.text_range(r).to_string())
            }
            Value::Comment | Value::CData | Value::ProcessingInstruction | Value::DocType => {
                let r = doc.flat[index].range.start as usize..doc.flat[index].range.end as usize;
                (Kind::Raw, r.clone(), doc.text_range(r).to_string())
            }
            Value::OpenElement { .. } | Value::EmptyElement { .. } => {
                let r = edit::attrs_blob_span(doc, index);
                (Kind::Attrs, r.clone(), doc.text_range(r).to_string())
            }
            Value::CloseElement { .. } => unreachable!("normalized above"),
        };

        let prompt = match kind {
            Kind::Text => "text: ",
            Kind::Raw => "content: ",
            Kind::Attrs => "attributes (raw, e.g. ` id=\"1\" active=\"true\"`): ",
        };
        let Some(new_value) = self.read_line(input, prompt, &current) else {
            return;
        };

        let focus_hint = range.start;
        let replacement = match kind {
            Kind::Text => edit::escape_text(&new_value).into_bytes(),
            Kind::Raw | Kind::Attrs => new_value.into_bytes(),
        };
        self.commit_edit(vec![(range, replacement)], focus_hint);
    }

    fn insert_sibling<I: Iterator<Item = std::io::Result<TuiEvent>>>(
        &mut self,
        input: &mut I,
        after: bool,
    ) {
        let Some(new_name) = self.read_line(input, "new element name: ", "new") else {
            return;
        };
        if new_name.is_empty() {
            return;
        }
        let span = edit::node_full_span(&self.viewer.doc, self.viewer.focused_row);
        let at = if after { span.end } else { span.start };
        let text = format!("<{new_name}/>");
        let focus_hint = at;
        self.commit_edit(vec![(at..at, text.into_bytes())], focus_hint);
    }

    fn paste_sibling(&mut self, after: bool) {
        let text = match clipboard::paste() {
            Ok(t) => t.trim().to_string(),
            Err(e) => {
                self.message = Some(format!("paste failed: {e}"));
                return;
            }
        };
        if text.is_empty() {
            self.message = Some("clipboard is empty".to_string());
            return;
        }
        let span = edit::node_full_span(&self.viewer.doc, self.viewer.focused_row);
        let at = if after { span.end } else { span.start };
        let focus_hint = at;
        self.commit_edit(vec![(at..at, text.into_bytes())], focus_hint);
    }

    fn delete_focused(&mut self) {
        let span = edit::node_full_span(&self.viewer.doc, self.viewer.focused_row);
        let focus_hint = edit::parent_or_self_offset(&self.viewer.doc, self.viewer.focused_row);
        self.commit_edit(vec![(span, Vec::new())], focus_hint);
    }

    fn do_undo(&mut self) {
        match self.edit_history.undo(&mut self.viewer) {
            // Row numbering changed (full re-parse) — see commit_edit's
            // comment on why current_match must be cleared here too.
            Ok(true) => {
                self.search.current_match = None;
                self.message = Some("undo".to_string());
            }
            Ok(false) => self.message = Some("nothing to undo".to_string()),
            Err(e) => self.message = Some(format!("undo failed: {e}")),
        }
    }

    fn do_redo(&mut self) {
        match self.edit_history.redo(&mut self.viewer) {
            Ok(true) => {
                self.search.current_match = None;
                self.message = Some("redo".to_string());
            }
            Ok(false) => self.message = Some("nothing to redo".to_string()),
            Err(e) => self.message = Some(format!("redo failed: {e}")),
        }
    }

    // --- Command mode ----------------------------------------------------

    fn command_mode<I: Iterator<Item = std::io::Result<TuiEvent>>>(
        &mut self,
        input: &mut I,
    ) -> Flow {
        let Some(command) = self.read_line(input, ":", "") else {
            return Flow::Continue;
        };
        let command = command.trim();

        if command == "q" || command == "quit" {
            if self.edit_history.is_dirty() {
                self.message = Some("unsaved changes — :w to save or :q! to discard".to_string());
            } else {
                return Flow::Quit;
            }
        } else if command == "q!" || command == "quit!" {
            return Flow::Quit;
        } else if command == "w" {
            self.save(None);
        } else if let Some(path) = command.strip_prefix("w ") {
            self.save(Some(PathBuf::from(path.trim())));
        } else if command == "wq" {
            self.save(None);
            if !self.edit_history.is_dirty() {
                return Flow::Quit;
            }
        } else if command == "set number" {
            self.screen_writer.show_line_numbers = true;
        } else if command == "set nonumber" {
            self.screen_writer.show_line_numbers = false;
        } else if command == "set relativenumber" {
            self.screen_writer.show_relative_line_numbers = true;
        } else if command == "set norelativenumber" {
            self.screen_writer.show_relative_line_numbers = false;
        } else if !command.is_empty() {
            self.message = Some(format!("unknown command: {command}"));
        }

        Flow::Continue
    }

    fn save(&mut self, path: Option<PathBuf>) {
        let target = match path.or_else(|| self.file_path.clone()) {
            Some(p) => p,
            None => {
                self.message = Some("no filename — use :w <path>".to_string());
                return;
            }
        };
        match self.edit_history.save(&self.viewer, &target) {
            Ok(()) => {
                self.message = Some(format!("saved {}", target.display()));
                self.file_path = Some(target);
                self.edit_history.mark_saved();
            }
            Err(e) => self.message = Some(format!("save failed: {e}")),
        }
    }

    fn draw(&mut self) {
        self.screen_writer.print(
            &self.viewer,
            &self.filename,
            self.edit_history.is_dirty(),
            &self.search,
            &self.message,
        );
    }
}

pub fn query_terminal_size() -> Option<TTYDimensions> {
    // Some pty setups (e.g. one never sent a TIOCSWINSZ) report a "valid"
    // but degenerate 0x0 size rather than erroring — guard against that
    // producing an unusable zero-height layout instead of just falling
    // through to main.rs's 80x24 default.
    termion::terminal_size().ok().and_then(|(width, height)| {
        if width == 0 || height == 0 {
            None
        } else {
            Some(TTYDimensions { width, height })
        }
    })
}
