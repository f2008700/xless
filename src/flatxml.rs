// The core data model: an XML document flattened into a `Vec<Row>`, one
// row per navigable "line" (element open/close, text, comment, CData,
// processing instruction, doctype).
//
// This is a close port of jless's `flatjson.rs` (~/git/jless/src/flatjson.rs)
// — same idea (a flattened tree with parent/sibling/pair links, navigated by
// index rather than by walking a real tree), same traversal algorithms
// (`next_visible_row`/`prev_visible_row` skip the body of collapsed
// containers, `next_item`/`prev_item` additionally skip closing rows) —
// adapted in two ways documented in README.md's Architecture section:
//
//   1. `Value` models XML's shape (elements w/ attributes, text, comments,
//      CDATA, PIs, doctype) instead of JSON's (objects/arrays/primitives).
//      Attributes are *not* separate rows — see README.md's Architecture section §3 — they
//      live in a side table (`FlatXml::attrs`) referenced by a `Range<u32>`
//      on the owning element.
//   2. Indices and byte ranges are `u32`, not `usize`/`Range<usize>`, and
//      `Row.range` points into the *original* source bytes, not a second
//      regenerated pretty-printed string — see README.md's Architecture section §8 (this is
//      the large-file, 100-200MB-target memory optimization).

use std::ops::Range;

pub type Index = u32;
pub const NIL: Index = u32::MAX;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OptionIndex {
    Nil,
    Index(Index),
}

impl OptionIndex {
    // Not called anywhere yet — kept as the natural counterpart to
    // `unwrap()`/`to_option()` below (ported from jless's flatjson.rs
    // OptionIndex, which has the same pair), for whichever future caller
    // wants a plain bool check instead of matching/converting.
    #[allow(dead_code)]
    pub fn is_nil(&self) -> bool {
        matches!(self, OptionIndex::Nil)
    }

    #[allow(dead_code)]
    pub fn is_some(&self) -> bool {
        !self.is_nil()
    }

    pub fn unwrap(&self) -> Index {
        match self {
            OptionIndex::Nil => panic!("called .unwrap() on a Nil OptionIndex"),
            OptionIndex::Index(i) => *i,
        }
    }

    pub fn to_option(self) -> Option<Index> {
        match self {
            OptionIndex::Nil => None,
            OptionIndex::Index(i) => Some(i),
        }
    }
}

impl From<Index> for OptionIndex {
    fn from(i: Index) -> Self {
        if i == NIL {
            OptionIndex::Nil
        } else {
            OptionIndex::Index(i)
        }
    }
}

/// One attribute on an element: `name="value"`. `value` is the *raw* (still
/// XML-escaped, unquoted) byte range of the value in the source — rendering
/// decides how to display it, we don't unescape at parse time (see
/// README.md's Architecture section §4 on keeping search/yank/rendering working on exactly
/// the text the user wrote).
#[derive(Clone, Debug)]
pub struct Attr {
    pub name: Range<u32>,
    pub value: Range<u32>,
}

#[derive(Debug)]
pub enum Value {
    /// Character data between tags.
    Text,
    /// `<![CDATA[ ... ]]>` — `Row.range` is the content, excluding the
    /// `<![CDATA[`/`]]>` delimiters.
    CData,
    /// `<!-- ... -->` — `Row.range` is the content, excluding `<!--`/`-->`.
    Comment,
    /// `<?target data?>`, including the XML declaration `<?xml ...?>` —
    /// `Row.range` is the raw content between `<?` and `?>`.
    ProcessingInstruction,
    /// `<!DOCTYPE ...>` — `Row.range` is the raw content between
    /// `<!DOCTYPE` and the closing `>`. Prolog-only, never has children.
    DocType,

    /// `<tag attr="value"/>` — an element with no content.
    EmptyElement { attrs: Range<u32> },

    /// `<tag attr="value">` — `Row.range` is just the tag name.
    ///
    /// No `first_child` field: a subtree is always the contiguous row
    /// slice `open..=close`, so the first child (if any) is simply the next
    /// row — see `FlatXml::first_child`. `<a></a>` (an open/close pair with
    /// nothing between, which xless doesn't fuse into `EmptyElement` the
    /// way jless fuses empty containers) is the `close_index == open + 1`
    /// case.
    OpenElement {
        collapsed: bool,
        attrs: Range<u32>,
        close_index: Index,
    },
    /// `</tag>` — `Row.range` is just the tag name.
    CloseElement { collapsed: bool, open_index: Index },
}

impl Value {
    pub fn is_primitive(&self) -> bool {
        !self.is_container()
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Value::OpenElement { .. } | Value::CloseElement { .. })
    }

    pub fn is_opening_of_container(&self) -> bool {
        matches!(self, Value::OpenElement { .. })
    }

    pub fn is_closing_of_container(&self) -> bool {
        matches!(self, Value::CloseElement { .. })
    }

    pub fn is_collapsed(&self) -> bool {
        match self {
            Value::OpenElement { collapsed, .. } => *collapsed,
            Value::CloseElement { collapsed, .. } => *collapsed,
            _ => false,
        }
    }

    pub fn is_expanded(&self) -> bool {
        !self.is_collapsed()
    }

    pub fn attrs(&self) -> Option<Range<u32>> {
        match self {
            Value::EmptyElement { attrs } => Some(attrs.clone()),
            Value::OpenElement { attrs, .. } => Some(attrs.clone()),
            _ => None,
        }
    }

    fn set_collapsed(&mut self, val: bool) {
        match self {
            Value::OpenElement { collapsed, .. } => *collapsed = val,
            Value::CloseElement { collapsed, .. } => *collapsed = val,
            _ => {}
        }
    }

    fn toggle_collapsed(&mut self) {
        self.set_collapsed(!self.is_collapsed())
    }

    fn pair_index(&self) -> OptionIndex {
        match self {
            Value::OpenElement { close_index, .. } => OptionIndex::Index(*close_index),
            Value::CloseElement { open_index, .. } => OptionIndex::Index(*open_index),
            _ => OptionIndex::Nil,
        }
    }
}

/// One row is 36 bytes. At 100–200MB inputs there are ~10–20M of these,
/// so every field counts: an earlier layout stored `prev_sibling`,
/// `next_sibling`, `first_child` and `last_child` as `OptionIndex` (an
/// 8-byte enum) — 64 bytes per row, ~0.9GB of index for a 150MB file.
/// All four are derivable in O(1) from the source-order, contiguous-
/// subtree layout (see `FlatXml::{prev_sibling, next_sibling,
/// first_child, last_child}`), and `parent` is stored as a raw `Index`
/// with `NIL` instead of an `OptionIndex`.
#[derive(Debug)]
pub struct Row {
    /// `NIL` at top level — read it through `Row::parent()`.
    parent: Index,

    pub depth: u32,
    pub index_in_parent: u32,

    /// For elements: just the tag name. For text/comment/CData/PI/doctype:
    /// the content. Byte range into the *original source*, not a
    /// regenerated string — see module doc comment.
    pub range: Range<u32>,

    pub value: Value,
}

impl Row {
    pub fn new(
        parent: OptionIndex,
        depth: u32,
        index_in_parent: u32,
        range: Range<u32>,
        value: Value,
    ) -> Row {
        Row {
            parent: match parent {
                OptionIndex::Index(p) => p,
                OptionIndex::Nil => NIL,
            },
            depth,
            index_in_parent,
            range,
            value,
        }
    }

    pub fn parent(&self) -> OptionIndex {
        OptionIndex::from(self.parent)
    }

    pub fn is_primitive(&self) -> bool {
        self.value.is_primitive()
    }
    pub fn is_container(&self) -> bool {
        self.value.is_container()
    }
    pub fn is_opening_of_container(&self) -> bool {
        self.value.is_opening_of_container()
    }
    pub fn is_closing_of_container(&self) -> bool {
        self.value.is_closing_of_container()
    }
    pub fn is_collapsed(&self) -> bool {
        self.value.is_collapsed()
    }
    pub fn is_expanded(&self) -> bool {
        self.value.is_expanded()
    }

    fn expand(&mut self) {
        self.value.set_collapsed(false);
    }
    fn collapse(&mut self) {
        self.value.set_collapsed(true);
    }
    fn toggle_collapsed(&mut self) {
        self.value.toggle_collapsed();
    }

    pub fn pair_index(&self) -> OptionIndex {
        self.value.pair_index()
    }
}

/// The flattened document: every row (in source order, including both
/// halves of every open/close element pair), plus the attribute side table
/// rows reference into. Does *not* own the source bytes — see
/// `crate::document::Document`, which pairs a `FlatXml` with the mmap'd or
/// owned buffer its ranges point into.
#[derive(Debug, Default)]
pub struct FlatXml {
    pub rows: Vec<Row>,
    pub attrs: Vec<Attr>,
}

impl std::ops::Index<Index> for FlatXml {
    type Output = Row;

    fn index(&self, index: Index) -> &Row {
        &self.rows[index as usize]
    }
}

impl std::ops::IndexMut<Index> for FlatXml {
    fn index_mut(&mut self, index: Index) -> &mut Row {
        &mut self.rows[index as usize]
    }
}

impl FlatXml {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    // Not called anywhere yet, but required alongside `len()` above to
    // satisfy clippy's `len_without_is_empty` convention (a type with
    // `len()` is expected to also offer `is_empty()`).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn attrs_of(&self, index: Index) -> &[Attr] {
        match self[index].value.attrs() {
            Some(r) => &self.attrs[r.start as usize..r.end as usize],
            None => &[],
        }
    }

    /// First child of an opening row — the very next row, unless that's
    /// its own closing row (no children). `Nil` for anything else.
    pub fn first_child(&self, index: Index) -> OptionIndex {
        match self[index].value {
            Value::OpenElement { close_index, .. } if close_index > index + 1 => {
                OptionIndex::Index(index + 1)
            }
            _ => OptionIndex::Nil,
        }
    }

    /// Last child of a closing row: the row just before it, normalized to
    /// its opening row if it's itself a closing tag. `Nil` for anything
    /// else, or an element with no children.
    pub fn last_child(&self, index: Index) -> OptionIndex {
        match self[index].value {
            Value::CloseElement { open_index, .. } if index > open_index + 1 => {
                self.node_start(index - 1)
            }
            _ => OptionIndex::Nil,
        }
    }

    /// Previous sibling of a node (closing rows aren't sibling-linked:
    /// `Nil`). The row before a node is either its parent's opening row
    /// (it's a first child), or the last row of its previous sibling —
    /// that sibling itself, or that sibling's closing tag.
    #[allow(dead_code)] // part of the link API; current callers only walk forward
    pub fn prev_sibling(&self, index: Index) -> OptionIndex {
        if index == 0 || self[index].is_closing_of_container() {
            return OptionIndex::Nil;
        }
        if self[index - 1].is_opening_of_container() {
            return OptionIndex::Nil;
        }
        self.node_start(index - 1)
    }

    /// Next sibling of a node (`Nil` for closing rows). The row after a
    /// node's last row is either its parent's closing tag (it's a last
    /// child), the end of the document, or the next sibling.
    pub fn next_sibling(&self, index: Index) -> OptionIndex {
        let last = match self[index].value {
            Value::CloseElement { .. } => return OptionIndex::Nil,
            Value::OpenElement { close_index, .. } => close_index,
            _ => index,
        };
        let next = last + 1;
        if next as usize >= self.rows.len() || self[next].is_closing_of_container() {
            OptionIndex::Nil
        } else {
            OptionIndex::Index(next)
        }
    }

    /// The node a row belongs to: a closing row's opening row, else itself.
    fn node_start(&self, index: Index) -> OptionIndex {
        match self[index].value {
            Value::CloseElement { open_index, .. } => OptionIndex::Index(open_index),
            _ => OptionIndex::Index(index),
        }
    }

    pub fn last_visible_index(&self) -> Index {
        let last_index = (self.rows.len() - 1) as Index;
        let row = &self[last_index];

        if row.is_container() && row.is_collapsed() {
            row.pair_index().unwrap()
        } else {
            last_index
        }
    }

    pub fn last_visible_item(&self) -> Index {
        let mut last_index = (self.rows.len() - 1) as Index;

        loop {
            let row = &self[last_index];

            if row.is_primitive() {
                return last_index;
            }

            if row.is_closing_of_container() && row.is_collapsed() {
                return row.pair_index().unwrap();
            }

            last_index -= 1;
        }
    }

    pub fn prev_visible_row(&self, index: Index) -> OptionIndex {
        if index == 0 {
            return OptionIndex::Nil;
        }

        let row = &self[index - 1];

        if row.is_closing_of_container() && row.is_collapsed() {
            row.pair_index()
        } else {
            OptionIndex::Index(index - 1)
        }
    }

    pub fn next_visible_row(&self, mut index: Index) -> OptionIndex {
        if self[index].is_opening_of_container() && self[index].is_collapsed() {
            index = self[index].pair_index().unwrap();
        }

        if index as usize == self.rows.len() - 1 {
            return OptionIndex::Nil;
        }

        OptionIndex::Index(index + 1)
    }

    pub fn prev_item(&self, mut index: Index) -> OptionIndex {
        while let OptionIndex::Index(i) = self.prev_visible_row(index) {
            if !self[i].is_closing_of_container() {
                return OptionIndex::Index(i);
            }
            index = i;
        }
        OptionIndex::Nil
    }

    pub fn next_item(&self, mut index: Index) -> OptionIndex {
        while let OptionIndex::Index(i) = self.next_visible_row(index) {
            if !self[i].is_closing_of_container() {
                return OptionIndex::Index(i);
            }
            index = i;
        }
        OptionIndex::Nil
    }

    pub fn expand(&mut self, index: Index) {
        if let OptionIndex::Index(pair) = self[index].pair_index() {
            self[pair].expand();
        }
        self[index].expand();
    }

    pub fn collapse(&mut self, index: Index) {
        if let OptionIndex::Index(pair) = self[index].pair_index() {
            self[pair].collapse();
        }
        self[index].collapse();
    }

    pub fn toggle_collapsed(&mut self, index: Index) {
        if let OptionIndex::Index(pair) = self[index].pair_index() {
            self[pair].toggle_collapsed();
        }
        self[index].toggle_collapsed();
    }

    // Ported from jless's flatjson.rs (same name/purpose: find the
    // nearest ancestor that's actually on screen when some ancestor is
    // collapsed) and covered by its own test, but not called by anything
    // yet — xless's current strategy for "make row X visible" is to
    // *expand* every collapsed ancestor instead (see
    // `Viewer::focus_row_expanding_ancestors`), which doesn't need this.
    // Kept as a documented building block for a future "jump to nearest
    // visible row without auto-expanding" mode.
    #[allow(dead_code)]
    pub fn first_visible_ancestor(&self, mut index: Index) -> Index {
        let mut visible_ancestor = index;
        while let OptionIndex::Index(parent) = self[index].parent() {
            if self[parent].is_collapsed() {
                visible_ancestor = parent;
            }
            index = parent;
        }
        visible_ancestor
    }

    /// Finds the row whose range contains, or most closely follows, the
    /// given byte offset in the original source. Used for `--focus-line`
    /// (README.md's Vim integration section) and, later, for the editing model's
    /// smallest-enclosing-element lookup (README.md's Editing model section §3).
    ///
    /// Binary search over row start offsets, since rows are in source
    /// order — but a plain "floor" search (the last row starting at or
    /// before `offset`) isn't right on its own: row ranges only cover a
    /// row's own token (a tag name, some text, ...), not the surrounding
    /// whitespace/punctuation, so an offset that lands in, say, a line's
    /// leading indentation — which is exactly what happens when
    /// `--focus-line` points at the *start* of a line — falls in the gap
    /// between the previous row's end and the next row's start. Floor
    /// search would then incorrectly resolve to the previous row (e.g.
    /// the parent's opening tag one line up) instead of whatever actually
    /// starts the target line. So: prefer the floor candidate only when
    /// `offset` actually falls inside its range; otherwise prefer the
    /// next row after the gap, since that's the row the target line
    /// visually starts with.
    pub fn row_at_source_offset(&self, offset: u32) -> Index {
        match self.rows.binary_search_by_key(&offset, |r| r.range.start) {
            Ok(i) => i as Index,
            Err(0) => 0,
            Err(i) => {
                let floor = (i - 1) as Index;
                if offset < self[floor].range.end {
                    floor
                } else if i < self.rows.len() {
                    i as Index
                } else {
                    floor
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmlparser;

    fn parse(xml: &str) -> FlatXml {
        xmlparser::parse(xml.as_bytes()).expect("parse failed")
    }

    const SIMPLE: &str = r#"<root>
        <a>1</a>
        <b>
            <c>3</c>
            <d>4</d>
        </b>
        <e>5</e>
    </root>"#;

    #[test]
    fn test_row_at_source_offset_lands_on_indentation() {
        // Regression test: an offset in a line's leading whitespace (e.g.
        // what --focus-line resolves a line number to) must resolve to
        // the row that *starts* that line, not the previous row, even
        // though no row's range covers indentation whitespace itself.
        let xml = "<root>\n  <a>\n    <b>\n      <c>text</c>\n    </b>\n  </a>\n</root>";
        let fj = parse(xml);
        // Byte offset of the start of the "      <c>text</c>" line (right
        // after the 3rd '\n'), i.e. pointing at its leading spaces.
        let line4_start = xml.match_indices('\n').nth(2).unwrap().0 as u32 + 1;
        let row = fj.row_at_source_offset(line4_start);
        // Should resolve to <c>'s opening row, not <b>'s.
        let tag = &xml.as_bytes()[fj[row].range.start as usize..fj[row].range.end as usize];
        assert_eq!(
            tag, b"c",
            "expected offset in line 4's indentation to resolve to <c>, not <b>"
        );
    }

    #[test]
    fn test_derived_links_match_brute_force() {
        // prev/next sibling and first/last child aren't stored — they're
        // derived from row order (see `Row`). Check them against a
        // brute-force reference built from `parent()` alone, on shapes
        // that exercise every edge: mixed content, `<a></a>` (open/close
        // with no children), deep nesting, prolog/epilog siblings of the
        // root.
        for xml in [
            SIMPLE,
            "<p>Hello <b>world</b>!<i/></p>",
            "<?xml version=\"1.0\"?><!--c--><r><a></a><b><c><d/></c></b>t</r><!--e--><?pi x?>",
            "<r><a><b><c><d>deep</d></c></b></a><e></e></r>",
        ] {
            let fj = parse(xml);
            let n = fj.len() as Index;
            let children = |p: OptionIndex| -> Vec<Index> {
                (0..n)
                    .filter(|&i| fj[i].parent() == p && !fj[i].is_closing_of_container())
                    .collect()
            };
            for i in 0..n {
                let row = &fj[i];
                if row.is_closing_of_container() {
                    assert_eq!(fj.prev_sibling(i), OptionIndex::Nil, "{xml} row {i}");
                    assert_eq!(fj.next_sibling(i), OptionIndex::Nil, "{xml} row {i}");
                    let kids = children(row.pair_index());
                    let expect = kids
                        .last()
                        .map_or(OptionIndex::Nil, |&k| OptionIndex::Index(k));
                    assert_eq!(fj.last_child(i), expect, "{xml} last_child of {i}");
                    continue;
                }
                let sibs = children(row.parent());
                let pos = sibs.iter().position(|&s| s == i).unwrap();
                assert_eq!(
                    row.index_in_parent as usize, pos,
                    "{xml} index_in_parent {i}"
                );
                let prev = if pos == 0 {
                    OptionIndex::Nil
                } else {
                    OptionIndex::Index(sibs[pos - 1])
                };
                let next = sibs
                    .get(pos + 1)
                    .map_or(OptionIndex::Nil, |&s| OptionIndex::Index(s));
                assert_eq!(fj.prev_sibling(i), prev, "{xml} prev_sibling {i}");
                assert_eq!(fj.next_sibling(i), next, "{xml} next_sibling {i}");
                let first = if row.is_opening_of_container() {
                    children(OptionIndex::Index(i))
                        .first()
                        .map_or(OptionIndex::Nil, |&k| OptionIndex::Index(k))
                } else {
                    OptionIndex::Nil
                };
                assert_eq!(fj.first_child(i), first, "{xml} first_child {i}");
            }
        }
    }

    #[test]
    fn test_row_is_compact() {
        // The row index dominates memory on large files (~14M rows for
        // 150MB of typical XML) — guard against accidental growth.
        assert!(
            std::mem::size_of::<Row>() <= 36,
            "Row is {} bytes",
            std::mem::size_of::<Row>()
        );
    }

    #[test]
    fn test_move_by_visible_rows() {
        let fj = parse(SIMPLE);
        // root(0) a-open(1) a-text(2) a-close(3) b-open(4) c-open(5)
        // c-text(6) c-close(7) d-open(8) d-text(9) d-close(10) b-close(11)
        // e-open(12) e-text(13) e-close(14) root-close(15)
        assert_visited_rows(
            &fj,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, NIL],
        );
    }

    #[test]
    fn test_move_by_visible_rows_collapsed() {
        let mut fj = parse(SIMPLE);
        fj.collapse(4); // collapse <b>
        assert_visited_rows(&fj, vec![1, 2, 3, 4, 12, 13, 14, 15, NIL]);
    }

    #[test]
    fn test_move_by_items() {
        let fj = parse(SIMPLE);
        // next_item only skips *closing* rows, not text rows — so every
        // open/text row shows up, just with consecutive closing rows
        // (e.g. d-close then b-close at 10,11) collapsed through.
        assert_visited_items(&fj, vec![1, 2, 4, 5, 6, 8, 9, 12, 13, NIL]);
    }

    #[test]
    fn test_first_visible_ancestor() {
        let mut fj = parse(SIMPLE);
        assert_eq!(fj.first_visible_ancestor(6), 6);
        fj.collapse(5); // collapse <c>
        assert_eq!(fj.first_visible_ancestor(6), 5);
        fj.collapse(4); // collapse <b>
        assert_eq!(fj.first_visible_ancestor(6), 4);
    }

    #[test]
    fn test_collapse_pairs_stay_in_sync() {
        let mut fj = parse(SIMPLE);
        fj.collapse(1); // collapse <a> (open at 1, close at 3)
        assert!(fj[1].is_collapsed());
        assert!(fj[3].is_collapsed());
        fj.expand(3);
        assert!(fj[1].is_expanded());
        assert!(fj[3].is_expanded());
    }

    fn assert_visited_rows(fj: &FlatXml, expected: Vec<u32>) {
        assert_row_iter(fj, 0, &expected, FlatXml::next_visible_row);
    }

    fn assert_visited_items(fj: &FlatXml, expected: Vec<u32>) {
        assert_row_iter(fj, 0, &expected, FlatXml::next_item);
    }

    fn assert_row_iter(
        fj: &FlatXml,
        start_index: Index,
        expected_visited_rows: &[u32],
        movement_fn: fn(&FlatXml, Index) -> OptionIndex,
    ) {
        let mut curr_index = start_index;
        for expected_index in expected_visited_rows.iter() {
            let next_index = movement_fn(fj, curr_index).to_option().unwrap_or(NIL);
            assert_eq!(next_index, *expected_index, "from {curr_index}");
            curr_index = next_index;
        }
    }
}
