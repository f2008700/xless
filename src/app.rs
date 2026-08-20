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
use crate::config::{self, AppAction, YankTarget};
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
    keymap: config::Keymap,
    search: SearchState,
    edit_history: EditHistory,
    count_buffer: String,
    /// Only ever `Some(AppAction::DeletePrefix)` or
    /// `Some(AppAction::YankPrefix)` — see `handle_prefixed_key`.
    pending: Option<AppAction>,
    message: Option<String>,
    showing_help: bool,
    /// First line of the (freshly-generated-per-frame — see
    /// `build_help_text`) help text currently shown at the top of the
    /// screen.
    help_scroll: u16,
    /// Which flavor the header bar (screenwriter.rs's
    /// `print_header_into_buffer`) shows — toggled with `X`.
    path_style: path::PathStyle,
}

impl<W: IoWrite> App<W> {
    pub fn new(
        viewer: Viewer,
        filename: String,
        file_path: Option<PathBuf>,
        keymap: config::Keymap,
        stdout: W,
    ) -> App<W> {
        let dimensions = viewer.dimensions;
        App {
            viewer,
            screen_writer: ScreenWriter::new(stdout, dimensions),
            filename,
            file_path,
            keymap,
            search: SearchState::default(),
            edit_history: EditHistory::default(),
            count_buffer: String::new(),
            pending: None,
            message: None,
            showing_help: false,
            help_scroll: 0,
            path_style: path::PathStyle::XPath,
        }
    }

    /// Regenerated on demand (cheap — a few dozen short strings), rather
    /// than built once and cached, specifically so it always reflects
    /// whatever's *actually* bound right now — the whole point of
    /// driving `:help` from `config::ACTIONS`/`self.keymap` instead of a
    /// static string table, which is what this used to be and which had
    /// no way to ever agree with a user's remapped keys.
    fn build_help_text(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(
            "xless — keyboard reference        (j/k/arrows scroll, q/Esc to close)".to_string(),
        );
        lines.push(String::new());

        let mut last_section = "";
        for info in config::ACTIONS {
            if info.section != last_section {
                if !last_section.is_empty() {
                    lines.push(String::new());
                }
                lines.push(info.section.to_string());
                last_section = info.section;
            }

            let keys = self.keymap.keys_for(info.action);
            let key_str = if keys.is_empty() {
                "(unbound)".to_string()
            } else {
                keys.iter()
                    .map(config::key_spec_to_string)
                    .collect::<Vec<_>>()
                    .join(" / ")
            };
            lines.push(format!("  {key_str:<18} {}", info.description));

            // The yank-target sub-keys are a separate map
            // (config::YANK_TARGETS), pressed *after* whatever key(s)
            // trigger YankPrefix — list them right under it rather than
            // as their own top-level ACTIONS entries, since on their own
            // they're not reachable from Normal mode at all.
            if info.action == AppAction::YankPrefix {
                for target_info in config::YANK_TARGETS {
                    let key_str = match self.keymap.key_for_yank_target(target_info.target) {
                        Some(k) => config::key_spec_to_string(&k),
                        None => "(unbound)".to_string(),
                    };
                    lines.push(format!(
                        "    then {key_str:<13} {}",
                        target_info.description
                    ));
                }
            }
        }

        lines.push(String::new());
        lines.push("Command mode (:)".to_string());
        lines.push("  :w [path]     save (to path, or the current file)".to_string());
        lines.push("  :wq           save and quit".to_string());
        lines.push("  :q            quit (warns if there are unsaved edits)".to_string());
        lines.push("  :q!           quit, discarding unsaved edits".to_string());
        lines.push("  :set number / nonumber              toggle line numbers".to_string());
        lines.push(
            "  :set relativenumber / norelativenumber   toggle relative line numbers".to_string(),
        );
        lines.push("  :h / :help    show this screen".to_string());
        lines.push(String::new());
        lines.push("Mouse: click focuses a row, wheel scrolls.".to_string());
        lines.push(
            "Esc: cancel a pending count/prefix. Ctrl-c: force quit. (Both always active, not configurable.)"
                .to_string(),
        );
        lines.push(format!(
            "Settings file: {} — see docs/CONFIG.md",
            config::default_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unavailable — $HOME not set)".to_string())
        ));

        lines
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

            // While the help screen is up, it owns input and behaves like
            // its own little pager — j/k/arrows/Ctrl-d/Ctrl-u/g/G scroll
            // it, `q`/Esc/Ctrl-c close it back to the normal view, and
            // anything else is just ignored. This mirrors `less`/`man`'s
            // own help (itself a navigable pager you quit with `q`), not
            // "any key closes it" — that first version was wrong: the
            // reference text is longer than one screen on a normal
            // terminal, so a key that happened to be a scroll key (e.g.
            // the down arrow) closing the whole screen instead of
            // scrolling made the bottom half of it unreachable.
            if self.showing_help {
                match event {
                    TuiEvent::WinChEvent => {
                        if let Some(dims) = query_terminal_size() {
                            self.viewer.dimensions = dims;
                            self.screen_writer.dimensions = dims;
                        }
                        self.clamp_help_scroll();
                    }
                    TuiEvent::KeyEvent(key) => self.handle_help_key(key),
                    TuiEvent::MouseEvent(MouseEvent::Press(MouseButton::WheelUp, _, _)) => {
                        self.scroll_help(-3)
                    }
                    TuiEvent::MouseEvent(MouseEvent::Press(MouseButton::WheelDown, _, _)) => {
                        self.scroll_help(3)
                    }
                    TuiEvent::MouseEvent(_) | TuiEvent::Unknown(_) => {}
                }
                self.draw();
                continue;
            }

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

        // Hardcoded, never remappable — see config.rs's module doc
        // comment on why: an unconditional escape hatch that a broken or
        // overly-creative config can never take away.
        if key == Key::Ctrl('c') {
            return Flow::Quit;
        }
        if key == Key::Esc {
            self.count_buffer.clear();
            return Flow::Continue;
        }

        let Some(action) = self.keymap.actions.get(&key).copied() else {
            return Flow::Continue;
        };

        match action {
            AppAction::Quit => {
                if self.edit_history.is_dirty() {
                    self.message = Some(
                        "unsaved changes — :w to save, :q! to discard, or :wq to save & quit"
                            .to_string(),
                    );
                    return Flow::Continue;
                }
                return Flow::Quit;
            }

            AppAction::MoveDown => {
                let n = self.take_count();
                self.viewer.perform_action(Action::MoveDown(n));
            }
            AppAction::MoveUp => {
                let n = self.take_count();
                self.viewer.perform_action(Action::MoveUp(n));
            }
            AppAction::MoveLeft => self.viewer.perform_action(Action::MoveLeft),
            AppAction::MoveRight => self.viewer.perform_action(Action::MoveRight),
            AppAction::FocusParent => self.viewer.perform_action(Action::FocusParent),
            AppAction::FocusNextSibling => {
                let n = self.take_count();
                self.viewer.perform_action(Action::FocusNextSibling(n));
            }
            AppAction::FocusPrevSibling => {
                let n = self.take_count();
                self.viewer.perform_action(Action::FocusPrevSibling(n));
            }
            AppAction::FocusFirstSibling => self.viewer.perform_action(Action::FocusFirstSibling),
            AppAction::FocusLastSibling => self.viewer.perform_action(Action::FocusLastSibling),
            AppAction::FocusTop => {
                self.count_buffer.clear();
                self.viewer.perform_action(Action::FocusTop);
            }
            AppAction::FocusBottom => {
                if self.count_buffer.is_empty() {
                    self.viewer.perform_action(Action::FocusBottom);
                } else {
                    let n = self.take_count();
                    let row = (n as Index - 1).min(self.viewer.doc.flat.len() as Index - 1);
                    self.viewer.perform_action(Action::JumpTo(row));
                }
            }
            AppAction::FocusMatchingPair => self.viewer.perform_action(Action::FocusMatchingPair),
            AppAction::ToggleCollapsed => self.viewer.perform_action(Action::ToggleCollapsed),
            AppAction::CollapseNodeAndSiblings => {
                self.viewer.perform_action(Action::CollapseNodeAndSiblings)
            }
            AppAction::ExpandNodeAndSiblings => {
                self.viewer.perform_action(Action::ExpandNodeAndSiblings)
            }
            AppAction::ToggleMode => self.viewer.perform_action(Action::ToggleMode),
            AppAction::TogglePathStyle => {
                self.path_style = self.path_style.toggled();
                self.message = Some(format!("header now showing {}", self.path_style.label()));
            }
            AppAction::PageDown => {
                let n = self.take_count();
                self.viewer.perform_action(Action::PageDown(n));
            }
            AppAction::PageUp => {
                let n = self.take_count();
                self.viewer.perform_action(Action::PageUp(n));
            }
            AppAction::ScrollDown => self.viewer.perform_action(Action::ScrollDown(1)),
            AppAction::ScrollUp => self.viewer.perform_action(Action::ScrollUp(1)),
            AppAction::MoveFocusedLineToTop => {
                self.viewer.perform_action(Action::MoveFocusedLineToTop)
            }
            AppAction::MoveFocusedLineToCenter => {
                self.viewer.perform_action(Action::MoveFocusedLineToCenter)
            }
            AppAction::MoveFocusedLineToBottom => {
                self.viewer.perform_action(Action::MoveFocusedLineToBottom)
            }

            AppAction::DeletePrefix => self.pending = Some(AppAction::DeletePrefix),
            AppAction::YankPrefix => self.pending = Some(AppAction::YankPrefix),

            AppAction::Rename => self.rename_focused(input),
            AppAction::EditContent => self.edit_content(input),
            AppAction::InsertAfter => self.insert_sibling(input, true),
            AppAction::InsertBefore => self.insert_sibling(input, false),
            AppAction::PasteAfter => self.paste_sibling(true),
            AppAction::PasteBefore => self.paste_sibling(false),
            AppAction::Undo => self.do_undo(),
            AppAction::Redo => self.do_redo(),

            AppAction::SearchForward => self.start_search(input, Direction::Forward),
            AppAction::SearchBackward => self.start_search(input, Direction::Reverse),
            // search_next repeats the last search in whichever direction
            // it was originally made ('?' searches stay "backwards" on
            // search_next); search_prev repeats it in the opposite
            // direction — vim's convention, not "next=forward/
            // prev=backward" regardless of how the search started.
            AppAction::SearchNext => self.jump_to_match(self.search.direction),
            AppAction::SearchPrev => self.jump_to_match(self.search.direction.reversed()),

            AppAction::CommandMode => return self.command_mode(input),
        }

        Flow::Continue
    }

    fn handle_prefixed_key(&mut self, prefix: AppAction, key: Key) -> Flow {
        match prefix {
            AppAction::DeletePrefix => {
                // vim's "dd" convention, generalized to whatever key(s)
                // are actually configured for delete_node: pressing it
                // again confirms, matching any other key cancels.
                if self.keymap.actions.get(&key) == Some(&AppAction::DeletePrefix) {
                    self.delete_focused();
                }
            }
            AppAction::YankPrefix => {
                if let Some(target) = self.keymap.yank_targets.get(&key).copied() {
                    match target {
                        YankTarget::Pretty => self.yank(crate::lineprinter::pretty_printed_subtree),
                        YankTarget::OneLine => self.yank(crate::lineprinter::one_line_subtree),
                        YankTarget::TextContent => self.yank(crate::lineprinter::text_content),
                        YankTarget::TagName => self.yank(|doc, i| doc.row_text(i).to_string()),
                        YankTarget::XPath => self.yank_xpath(),
                    }
                }
            }
            // `pending` is only ever set to one of the two arms above.
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
        } else if command == "h" || command == "help" {
            self.showing_help = true;
            self.help_scroll = 0;
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

    // --- Help screen (a tiny pager of its own) ---------------------------

    fn handle_help_key(&mut self, key: Key) {
        match key {
            Key::Char('q') | Key::Esc | Key::Ctrl('c') => self.showing_help = false,
            Key::Char('j') | Key::Down => self.scroll_help(1),
            Key::Char('k') | Key::Up => self.scroll_help(-1),
            Key::Char(' ') | Key::Ctrl('d') | Key::PageDown => {
                self.scroll_help(self.help_page_size())
            }
            Key::Ctrl('u') | Key::PageUp => self.scroll_help(-self.help_page_size()),
            Key::Char('g') | Key::Home => self.help_scroll = 0,
            Key::Char('G') | Key::End => {
                self.help_scroll = self.help_max_scroll();
            }
            _ => {}
        }
    }

    fn help_page_size(&self) -> i32 {
        self.screen_writer.dimensions.without_status_bar().height as i32
    }

    fn help_max_scroll(&self) -> u16 {
        let content_height = self.screen_writer.dimensions.without_status_bar().height as usize;
        self.build_help_text().len().saturating_sub(content_height) as u16
    }

    fn scroll_help(&mut self, delta: i32) {
        let new = (self.help_scroll as i32 + delta).max(0) as u16;
        self.help_scroll = new.min(self.help_max_scroll());
    }

    fn clamp_help_scroll(&mut self) {
        self.help_scroll = self.help_scroll.min(self.help_max_scroll());
    }

    fn draw(&mut self) {
        if self.showing_help {
            let text = self.build_help_text();
            self.screen_writer
                .print_help(&text, self.help_scroll as usize);
        } else {
            self.screen_writer.print(
                &self.viewer,
                &self.filename,
                self.edit_history.is_dirty(),
                &self.search,
                &self.message,
                self.path_style,
            );
        }
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
