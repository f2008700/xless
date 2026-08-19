// Navigation/collapse state machine. Adapted from jless's src/viewer.rs
// (~/git/jless/src/viewer.rs, MIT — see THIRD_PARTY_NOTICES.md): the
// `Action` enum and the movement/scrolling algorithms below are a close
// port (docs/ARCHITECTURE.md §6 calls this an "adapt," not a "rewrite,"
// because the state machine itself is format-agnostic — only the
// `FlatJson`→`FlatXml` swap is XML-specific).
//
// Mode::Line / Mode::Compact mirrors jless's Mode::Line/Mode::Data: it
// changes both rendering (lineprinter.rs) and which traversal function
// movement uses — `next_visible_row`/`prev_visible_row` (closing tags are
// independently visible rows) vs `next_item`/`prev_item` (closing tags are
// always skipped, matching jless's Data mode where "closing braces ...
// are elided" unconditionally, not just when a container is empty).
//
// v1 simplification vs. ARCHITECTURE.md §5's original table: Line mode
// here does *not* put each attribute on its own physical line — doing
// that would mean a single logical Row spans multiple *terminal* lines,
// which breaks the one-row-one-line invariant screenwriter.rs (and this
// whole traversal model) relies on. Line mode instead means "closing tags
// and genuinely-empty elements are always shown as real, independently
// navigable rows" (full structural fidelity), same spirit as jless's
// Line/Data distinction, just without the multi-line-per-row attribute
// wrapping. Attributes stay inline in both modes.

use crate::document::Document;
use crate::flatxml::{Index, OptionIndex};
use crate::types::TTYDimensions;

const DEFAULT_SCROLLOFF: u16 = 3;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Mode {
    Line,
    Compact,
}

impl Mode {
    pub fn toggled(self) -> Mode {
        match self {
            Mode::Line => Mode::Compact,
            Mode::Compact => Mode::Line,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Line => "LINE",
            Mode::Compact => "COMPACT",
        }
    }
}

/// Owns the document (source bytes + flattened row tree, via `Document`)
/// together with cursor/scroll/collapse view state. These are combined
/// into one struct (unlike jless, which keeps `JsonViewer` and its input
/// `FlatJson` a bit more separable) because rendering needs `Document`'s
/// source text *and* `FlatXml`'s structure together, while collapse/
/// expand needs mutable access to that same `FlatXml` — splitting them
/// into two top-level structs just pushes the coupling into borrow-checker
/// fights at every call site instead of removing it.
pub struct Viewer {
    pub doc: Document,
    pub top_row: Index,
    pub focused_row: Index,

    desired_depth: u32,
    pub dimensions: TTYDimensions,
    /// How many rows of `dimensions.height` are screenwriter chrome (the
    /// status bar, and — once app.rs turns it on — the path header),
    /// never document content. All the scrolling/paging math below reads
    /// `dimensions.height` loosely as "how many rows are on screen"
    /// (matching jless's own `JsonViewer`, which does the same rather
    /// than threading an exact content height through every formula), so
    /// this is a single knob screenwriter.rs's row budget stays in sync
    /// with, instead of `Viewer` hardcoding "1, for the status bar" the
    /// way an earlier version did — that would have silently broken
    /// scrolling (off by one screen row) the moment a second chrome row
    /// was added. See `content_height()`.
    pub reserved_rows: u16,
    pub scrolloff_setting: u16,
    pub mode: Mode,
}

impl Viewer {
    pub fn new(doc: Document, dimensions: TTYDimensions) -> Viewer {
        Viewer {
            doc,
            top_row: 0,
            focused_row: 0,
            desired_depth: 0,
            dimensions,
            reserved_rows: 1,
            scrolloff_setting: DEFAULT_SCROLLOFF,
            mode: Mode::Compact,
        }
    }

    /// Rows actually available for document content — `dimensions.height`
    /// minus whatever screenwriter chrome is reserved (see
    /// `reserved_rows`). Every scrolling/paging calculation below should
    /// read this, not `dimensions.height` directly.
    fn content_height(&self) -> u16 {
        self.dimensions.height.saturating_sub(self.reserved_rows)
    }

    /// Jump straight to (and expand every collapsed ancestor of) whichever
    /// row contains the given byte offset in the original source. Backs
    /// `--focus-line`/`--focus-pos` (docs/VIM_INTEGRATION.md §2).
    pub fn focus_source_offset(&mut self, offset: u32) {
        let row = self.doc.flat.row_at_source_offset(offset);
        self.focus_row_expanding_ancestors(row);
        self.move_focused_line_to_center();
    }

    /// Focuses `row` directly, expanding any collapsed ancestor so it's
    /// actually visible. Shared by `focus_source_offset` and search
    /// (app.rs) — a search match inside a collapsed element needs the
    /// same "make it visible" treatment `--focus-line` does.
    pub fn focus_row_expanding_ancestors(&mut self, row: Index) {
        let mut ancestor = row;
        while let OptionIndex::Index(parent) = self.doc.flat[ancestor].parent {
            if self.doc.flat[parent].is_collapsed() {
                self.doc.flat.expand(parent);
            }
            ancestor = parent;
        }
        self.focused_row = row;
        self.desired_depth = self.doc.flat[row].depth;
    }

    /// The next row that's actually displayed as its own line under the
    /// current mode (screenwriter.rs uses this to step through the
    /// visible frame; internally, movement uses the same function via the
    /// private alias below so the two can never drift apart).
    pub fn next_display_row(&self, index: Index) -> OptionIndex {
        match self.mode {
            Mode::Line => self.doc.flat.next_visible_row(index),
            Mode::Compact => self.doc.flat.next_item(index),
        }
    }

    fn next_row(&self, index: Index) -> OptionIndex {
        self.next_display_row(index)
    }

    fn prev_row(&self, index: Index) -> OptionIndex {
        match self.mode {
            Mode::Line => self.doc.flat.prev_visible_row(index),
            Mode::Compact => self.doc.flat.prev_item(index),
        }
    }

    fn last_line(&self) -> Index {
        match self.mode {
            Mode::Line => self.doc.flat.last_visible_index(),
            Mode::Compact => self.doc.flat.last_visible_item(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum Action {
    MoveUp(usize),
    MoveDown(usize),
    MoveLeft,
    MoveRight,

    FocusParent,
    FocusPrevSibling(usize),
    FocusNextSibling(usize),
    FocusFirstSibling,
    FocusLastSibling,
    FocusTop,
    FocusBottom,
    FocusMatchingPair,

    ScrollUp(usize),
    ScrollDown(usize),
    PageUp(usize),
    PageDown(usize),

    MoveFocusedLineToTop,
    MoveFocusedLineToCenter,
    MoveFocusedLineToBottom,

    Click(u16),

    ToggleCollapsed,
    CollapseNodeAndSiblings,
    ExpandNodeAndSiblings,
    ToggleMode,

    /// Focus a specific row directly (search results, `gg`+count, etc.),
    /// expanding collapsed ancestors first so it's actually visible.
    JumpTo(Index),

    ResizeViewerDimensions(TTYDimensions),
}

impl Viewer {
    pub fn perform_action(&mut self, action: Action) {
        let track_window = Viewer::should_refocus_window(&action);
        let reset_desired_depth = Viewer::should_reset_desired_depth(&action);

        match action {
            Action::MoveUp(n) => self.move_up(n),
            Action::MoveDown(n) => self.move_down(n),
            Action::MoveLeft => self.move_left(),
            Action::MoveRight => self.move_right(),
            Action::FocusParent => self.focus_parent(),
            Action::FocusPrevSibling(n) => self.focus_prev_sibling(n),
            Action::FocusNextSibling(n) => self.focus_next_sibling(n),
            Action::FocusFirstSibling => self.focus_first_sibling(),
            Action::FocusLastSibling => self.focus_last_sibling(),
            Action::FocusTop => self.focus_top(),
            Action::FocusBottom => self.focus_bottom(),
            Action::FocusMatchingPair => self.focus_matching_pair(),
            Action::ScrollUp(n) => self.scroll_up(n),
            Action::ScrollDown(n) => self.scroll_down(n),
            Action::PageUp(n) => self.scroll_up(self.content_height() as usize * n),
            Action::PageDown(n) => self.scroll_down(self.content_height() as usize * n),
            Action::MoveFocusedLineToTop => self.move_focused_line_to_top(),
            Action::MoveFocusedLineToCenter => self.move_focused_line_to_center(),
            Action::MoveFocusedLineToBottom => self.move_focused_line_to_bottom(),
            Action::Click(row) => self.click_row(row),
            Action::ToggleCollapsed => self.toggle_collapsed(),
            Action::CollapseNodeAndSiblings => self.set_collapse_state_on_node_and_siblings(true),
            Action::ExpandNodeAndSiblings => self.set_collapse_state_on_node_and_siblings(false),
            Action::ToggleMode => self.mode = self.mode.toggled(),
            Action::JumpTo(row) => self.focus_row_expanding_ancestors(row),
            Action::ResizeViewerDimensions(dims) => self.dimensions = dims,
        }

        if reset_desired_depth {
            self.desired_depth = self.doc.flat[self.focused_row].depth;
        }

        if track_window {
            self.ensure_focused_row_is_visible();
        }
    }

    fn should_refocus_window(action: &Action) -> bool {
        !matches!(
            action,
            Action::ScrollUp(_)
                | Action::ScrollDown(_)
                | Action::PageUp(_)
                | Action::PageDown(_)
                | Action::MoveFocusedLineToTop
                | Action::MoveFocusedLineToCenter
                | Action::MoveFocusedLineToBottom
                | Action::CollapseNodeAndSiblings
                | Action::ExpandNodeAndSiblings
                | Action::FocusTop
        )
    }

    fn should_reset_desired_depth(action: &Action) -> bool {
        !matches!(
            action,
            Action::FocusPrevSibling(_)
                | Action::FocusNextSibling(_)
                | Action::ScrollUp(_)
                | Action::ScrollDown(_)
                | Action::MoveFocusedLineToTop
                | Action::MoveFocusedLineToCenter
                | Action::MoveFocusedLineToBottom
                | Action::ResizeViewerDimensions(_)
        )
    }

    fn move_up(&mut self, rows: usize) {
        let mut row = self.focused_row;
        for _ in 0..rows {
            match self.prev_row(row) {
                OptionIndex::Nil => break,
                OptionIndex::Index(i) => row = i,
            }
        }
        self.focused_row = row;
    }

    fn move_down(&mut self, rows: usize) {
        let mut row = self.focused_row;
        for _ in 0..rows {
            match self.next_row(row) {
                OptionIndex::Nil => break,
                OptionIndex::Index(i) => row = i,
            }
        }
        self.focused_row = row;
    }

    fn move_right(&mut self) {
        let row = &self.doc.flat[self.focused_row];
        if row.is_primitive() {
            return;
        }
        if row.is_collapsed() {
            self.doc.flat.expand(self.focused_row);
            return;
        }
        if row.is_opening_of_container() {
            if let OptionIndex::Index(child) = row.first_child() {
                self.focused_row = child;
            }
        } else {
            // Focused on an expanded closing tag; jump to the row just
            // before it (mirrors jless's move_right on a Line-mode
            // closing brace row).
            self.focused_row = self.doc.flat.prev_visible_row(self.focused_row).unwrap();
        }
    }

    fn move_left(&mut self) {
        let row = &self.doc.flat[self.focused_row];
        if row.is_container() && row.is_expanded() {
            self.doc.flat.collapse(self.focused_row);
            if self.doc.flat[self.focused_row].is_closing_of_container() {
                self.focused_row = self.doc.flat[self.focused_row].pair_index().unwrap();
            }
            return;
        }
        self.focus_parent();
    }

    fn focus_parent(&mut self) {
        if let OptionIndex::Index(parent) = self.doc.flat[self.focused_row].parent {
            self.focused_row = parent;
        }
    }

    fn focus_prev_sibling(&mut self, rows: usize) {
        for _ in 0..rows {
            self.move_up(1);
            let mut row = &self.doc.flat[self.focused_row];
            while row.depth > self.desired_depth {
                self.focused_row = row.parent.unwrap();
                row = &self.doc.flat[self.focused_row];
            }
        }
    }

    fn focus_next_sibling(&mut self, rows: usize) {
        for _ in 0..rows {
            let row = &self.doc.flat[self.focused_row];
            if row.depth == self.desired_depth && row.is_opening_of_container() && row.is_expanded()
            {
                self.focused_row = row.pair_index().unwrap();
            } else {
                self.move_down(1);
            }
        }
    }

    fn focus_first_sibling(&mut self) {
        match self.doc.flat[self.focused_row].parent {
            OptionIndex::Index(parent) => {
                if let OptionIndex::Index(first) = self.doc.flat[parent].first_child() {
                    self.focused_row = first;
                }
            }
            OptionIndex::Nil => self.focus_top(),
        }
    }

    fn focus_last_sibling(&mut self) {
        match self.doc.flat[self.focused_row].parent {
            OptionIndex::Index(parent) => {
                let close = self.doc.flat[parent].pair_index().unwrap();
                if let OptionIndex::Index(last) = self.doc.flat[close].last_child() {
                    self.focused_row = last;
                }
            }
            OptionIndex::Nil => {
                let last = self.doc.flat.last_visible_index();
                self.focused_row = if self.doc.flat[last].is_container() {
                    self.doc.flat[last].pair_index().unwrap()
                } else {
                    last
                };
            }
        }
    }

    fn focus_top(&mut self) {
        self.top_row = 0;
        self.focused_row = 0;
    }

    fn focus_bottom(&mut self) {
        self.focused_row = self.last_line();
    }

    fn focus_matching_pair(&mut self) {
        // In Compact mode closing tags are never independently visible
        // rows (next_item/prev_item always skip them), so jumping to one
        // here would leave the cursor on a row the rest of Compact-mode
        // navigation can't reach — same guard jless uses for Data mode.
        if self.mode == Mode::Compact {
            return;
        }
        let row = &self.doc.flat[self.focused_row];
        if row.is_collapsed() || row.is_primitive() {
            return;
        }
        if let OptionIndex::Index(pair) = row.pair_index() {
            self.focused_row = pair;
        }
    }

    fn click_row(&mut self, screen_row: u16) {
        self.focused_row = self.count_n_lines_past(self.top_row, screen_row as usize);
    }

    fn toggle_collapsed(&mut self) {
        let row = &self.doc.flat[self.focused_row];
        if row.is_primitive() {
            return;
        }
        if row.is_closing_of_container() {
            self.focused_row = self.doc.flat[self.focused_row].pair_index().unwrap();
        }
        self.doc.flat.toggle_collapsed(self.focused_row);
    }

    fn set_collapse_state_on_node_and_siblings(&mut self, collapsed: bool) {
        if self.doc.flat[self.focused_row].is_closing_of_container() {
            self.focused_row = self.doc.flat[self.focused_row].pair_index().unwrap();
        }

        let first_sibling = match self.doc.flat[self.focused_row].parent {
            OptionIndex::Index(parent) => self.doc.flat[parent].first_child().to_option(),
            OptionIndex::Nil => Some(0),
        };

        let mut next = first_sibling;
        while let Some(i) = next {
            if self.doc.flat[i].is_container() {
                if collapsed {
                    self.doc.flat.collapse(i);
                } else {
                    self.doc.flat.expand(i);
                }
            }
            next = self.doc.flat[i].next_sibling.to_option();
        }

        self.ensure_focused_row_is_visible();
    }

    fn scroll_up(&mut self, rows: usize) {
        self.top_row = self.count_n_lines_before(self.top_row, rows);
        let max_focused_row = self.count_n_lines_past(
            self.top_row,
            (self
                .dimensions
                .height
                .saturating_sub(self.scrolloff())
                .saturating_sub(1)) as usize,
        );
        if self.focused_row > max_focused_row {
            self.focused_row = max_focused_row;
        }
    }

    fn scroll_down(&mut self, rows: usize) {
        self.top_row = self.count_n_lines_past(self.top_row, rows);
        let first_focusable_row = self.count_n_lines_past(self.top_row, self.scrolloff() as usize);
        if self.focused_row < first_focusable_row {
            self.focused_row = first_focusable_row;
        }
    }

    fn move_focused_line_to_top(&mut self) {
        self.top_row = self.focused_row;
    }

    fn move_focused_line_to_center(&mut self) {
        self.top_row =
            self.count_n_lines_before(self.focused_row, self.content_height() as usize / 2);
    }

    fn move_focused_line_to_bottom(&mut self) {
        self.top_row = self.count_n_lines_before(self.focused_row, self.content_height() as usize);
    }

    fn scrolloff(&self) -> u16 {
        self.scrolloff_setting.min(self.content_height() / 2)
    }

    fn ensure_focused_row_is_visible(&mut self) {
        self.ensure_top_row_is_visible();

        let scrolloff = self.scrolloff();
        let max_padding = self.content_height().saturating_sub(scrolloff);
        let recenter_distance = self.content_height() + (self.content_height() / 3);

        let num_visible_before_focused =
            self.count_visible_rows_before(self.top_row, self.focused_row, recenter_distance + 1);

        if self.focused_row < self.top_row || num_visible_before_focused < scrolloff {
            self.top_row = self.count_n_lines_before(self.focused_row, scrolloff as usize);
        } else if num_visible_before_focused > max_padding {
            let refocus_padding = if num_visible_before_focused > recenter_distance {
                (self.content_height() * 2 / 3).min(max_padding)
            } else {
                scrolloff
            };

            let last_line = self.last_line();
            let lines_visible_before_eof =
                self.count_visible_rows_before(self.focused_row, last_line, refocus_padding + 1);
            let bottom_padding = refocus_padding.min(lines_visible_before_eof);

            self.top_row = self.count_n_lines_before(
                self.focused_row,
                self.content_height().saturating_sub(bottom_padding) as usize,
            );
        }
    }

    fn ensure_top_row_is_visible(&mut self) {
        if self.doc.flat[self.top_row].is_closing_of_container() {
            let opening = self.doc.flat[self.top_row].pair_index().unwrap();
            if self.doc.flat[opening].is_collapsed() {
                self.top_row = opening;
            }
        }

        let mut ancestor = self.top_row;
        while let OptionIndex::Index(ancestor_index) = self.doc.flat[ancestor].parent {
            if self.doc.flat[ancestor_index].is_collapsed() {
                self.top_row = ancestor_index;
            }
            ancestor = ancestor_index;
        }
    }

    fn count_n_lines_before(&self, mut start: Index, mut lines: usize) -> Index {
        while lines != 0 && start != 0 {
            start = self.prev_row(start).unwrap();
            lines -= 1;
        }
        start
    }

    fn count_n_lines_past(&self, mut start: Index, mut lines: usize) -> Index {
        while lines != 0 {
            match self.next_row(start) {
                OptionIndex::Nil => break,
                OptionIndex::Index(n) => start = n,
            }
            lines -= 1;
        }
        start
    }

    fn count_visible_rows_before(&self, mut start: Index, end: Index, max: u16) -> u16 {
        let mut num_visible: u16 = 0;
        while start < end && num_visible < max {
            num_visible += 1;
            start = self.next_row(start).unwrap();
        }
        num_visible
    }

    pub fn index_of_focused_row_on_screen(&self) -> u16 {
        self.count_visible_rows_before(self.top_row, self.focused_row, self.content_height())
    }
}
