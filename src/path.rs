// Builds an XPath-flavored path to a row — the XML analog of jless's
// `FlatJson::build_path_to_node` (~/git/jless/src/flatjson.rs), which
// builds jq-style paths (`.foo[3].bar`). See README.md's Architecture section §7.1:
// XPath's sibling-position convention is 1-based and counts only
// same-tag siblings (`item[2]` means "the 2nd `<item>` among its
// siblings," not "the 2nd child overall"), which is different enough
// from jless's all-siblings `index_in_parent` that it's computed per
// parent by `PathCache` (one pass over that parent's children, then
// memoized) rather than stored on every `Row`.
//
// v1 simplification: this always includes the `[n]` predicate, even when
// an element has no same-tag siblings (where strict "shortest unambiguous
// path" XPath tooling would omit it). Always-included is simpler, still
// unambiguous, and matches what e.g. browser devtools' "Copy XPath"
// produces — a convenience path for humans/copy-paste, not a spec-minimal
// XPath compiler. Attributes aren't reachable (no `@name` segments)
// because attributes aren't individually focusable yet (README.md's
// Architecture section §7.2/§7.3).

use std::collections::HashMap;

use crate::document::Document;
use crate::flatxml::{Index, OptionIndex, Value, NIL};

/// Which of the two path flavors below to show — the header bar's
/// toggle (app.rs's `X` key) between `build_xpath`'s formal
/// `/root[1]/child[2]` addressing and `build_literal_path`'s plainer
/// `root/child` breadcrumb.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PathStyle {
    XPath,
    Literal,
}

impl PathStyle {
    pub fn toggled(self) -> PathStyle {
        match self {
            PathStyle::XPath => PathStyle::Literal,
            PathStyle::Literal => PathStyle::XPath,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PathStyle::XPath => "XPath",
            PathStyle::Literal => "literal path",
        }
    }
}

/// Single entry point covering both flavors, and — unlike `build_xpath`
/// alone — never fails: a `DocType` row (the one case `build_xpath`
/// rejects, since it isn't addressable in real XPath) still gets a
/// displayable string, by falling back to showing `build_xpath`'s error
/// message in parens rather than nothing at all.
#[allow(dead_code)] // the header uses `PathCache::build_path`; kept for one-off callers
pub fn build_path(doc: &Document, index: Index, style: PathStyle) -> String {
    PathCache::default().build_path(doc, index, style)
}

/// Same-tag sibling positions for one parent's children, computed in a
/// single pass over that parent's child list.
struct SiblingTable {
    /// 1-based same-tag position, indexed by `Row.index_in_parent`
    /// (0 for non-element children, which never need one).
    positions: Vec<u32>,
    /// How many children carry each tag name.
    totals: HashMap<String, u32>,
}

/// Memoizes `SiblingTable`s per parent so building a path is O(depth)
/// rather than O(depth × siblings). That matters because the header bar
/// rebuilds the focused node's path on *every* redraw: without the cache,
/// a node near the end of a million-sibling list re-walked every
/// preceding sibling each frame (measured at ~12–72ms per keypress).
/// Tables are built lazily, only for parents actually on a path that's
/// been asked for, and must be discarded (`clear`) whenever the document
/// changes — row indices and tag names are only stable between edits.
#[derive(Default)]
pub struct PathCache {
    // Keyed by parent row index; `NIL` stands for the document's top level.
    tables: HashMap<Index, SiblingTable>,
}

impl PathCache {
    pub fn clear(&mut self) {
        self.tables.clear();
    }

    pub fn build_path(&mut self, doc: &Document, index: Index, style: PathStyle) -> String {
        match style {
            PathStyle::XPath => self
                .build_xpath(doc, index)
                .unwrap_or_else(|e| format!("({e})")),
            PathStyle::Literal => self.build_literal_path(doc, index),
        }
    }

    fn table(&mut self, doc: &Document, index: Index) -> &SiblingTable {
        let parent = doc.flat[index].parent();
        let key = match parent {
            OptionIndex::Index(p) => p,
            OptionIndex::Nil => NIL,
        };
        self.tables.entry(key).or_insert_with(|| {
            // Top-level nodes are siblings starting at row 0.
            let mut child = match parent {
                OptionIndex::Index(p) => doc.flat.first_child(p),
                OptionIndex::Nil => OptionIndex::Index(0),
            };
            let mut positions = Vec::new();
            let mut totals: HashMap<String, u32> = HashMap::new();
            while let OptionIndex::Index(c) = child {
                let pos = match element_tag(doc, c) {
                    Some(tag) => {
                        let n = totals.entry(tag.to_string()).or_insert(0);
                        *n += 1;
                        *n
                    }
                    None => 0,
                };
                positions.push(pos);
                child = doc.flat.next_sibling(c);
            }
            SiblingTable { positions, totals }
        })
    }

    /// 1-based position of `index` among its *same-tag* siblings.
    fn same_tag_position(&mut self, doc: &Document, index: Index) -> usize {
        let i = doc.flat[index].index_in_parent as usize;
        (self.table(doc, index).positions[i] as usize).max(1)
    }

    /// True if no sibling (in either direction) shares `index`'s tag — i.e.
    /// `same_tag_position` would always be 1 for it, so a `[1]` predicate
    /// would carry no information.
    fn is_unique_among_siblings(&mut self, doc: &Document, index: Index) -> bool {
        let Some(tag) = element_tag(doc, index) else {
            return true;
        };
        self.table(doc, index).totals.get(tag).copied().unwrap_or(0) <= 1
    }

    /// See the free function `build_xpath`.
    pub fn build_xpath(&mut self, doc: &Document, index: Index) -> Result<String, String> {
        let index = normalize(doc, index);

        let leaf_node_test = match &doc.flat[index].value {
            Value::Text => Some("text()"),
            Value::CData => Some("text()"),
            Value::Comment => Some("comment()"),
            Value::ProcessingInstruction => Some("processing-instruction()"),
            Value::DocType => {
                return Err("a DOCTYPE declaration has no XPath representation".to_string());
            }
            _ => None,
        };

        let mut segments: Vec<String> = Vec::new();
        let mut element = match leaf_node_test {
            Some(node_test) => {
                segments.push(node_test.to_string());
                doc.flat[index].parent()
            }
            None => OptionIndex::Index(index),
        };
        while let OptionIndex::Index(e) = element {
            let tag = element_tag(doc, e).unwrap_or("?");
            let pos = self.same_tag_position(doc, e);
            segments.push(format!("{tag}[{pos}]"));
            element = doc.flat[e].parent();
        }

        segments.reverse();
        Ok(format!("/{}", segments.join("/")))
    }

    /// See the free function `build_literal_path`.
    pub fn build_literal_path(&mut self, doc: &Document, index: Index) -> String {
        let index = normalize(doc, index);

        let leaf_label = match &doc.flat[index].value {
            Value::Text => Some("text"),
            Value::CData => Some("CDATA"),
            Value::Comment => Some("comment"),
            Value::ProcessingInstruction => Some("PI"),
            Value::DocType => Some("DOCTYPE"),
            _ => None,
        };

        let mut segments: Vec<String> = Vec::new();
        let mut element = match leaf_label {
            Some(label) => {
                segments.push(label.to_string());
                doc.flat[index].parent()
            }
            None => OptionIndex::Index(index),
        };
        while let OptionIndex::Index(e) = element {
            let tag = element_tag(doc, e).unwrap_or("?");
            if self.is_unique_among_siblings(doc, e) {
                segments.push(tag.to_string());
            } else {
                segments.push(format!("{tag}[{}]", self.same_tag_position(doc, e)));
            }
            element = doc.flat[e].parent();
        }

        segments.reverse();
        segments.join("/")
    }
}

fn element_tag(doc: &Document, index: Index) -> Option<&str> {
    match &doc.flat[index].value {
        Value::OpenElement { .. } | Value::EmptyElement { .. } => Some(doc.row_text(index)),
        _ => None,
    }
}

/// A closing-tag row and its opening row are the same logical node.
fn normalize(doc: &Document, index: Index) -> Index {
    if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    }
}

/// Builds `/root/child[2]/grandchild` (or `/root/child[2]/text()` etc. for
/// a non-element leaf) addressing `index`. Normalizes a closing-tag row to
/// its opening row first, since they represent the same logical node.
///
/// Returns `Err` for a `DocType` row: unlike `text()`/`comment()`/
/// `processing-instruction()`, a `<!DOCTYPE ...>` declaration isn't a
/// node in the XPath data model at all, so there's no legal node test to
/// address it with. An earlier version of this function fabricated the
/// placeholder string `"!DOCTYPE"` here, which isn't valid XPath syntax —
/// found by review: `yx` on a DocType row would silently copy a path a
/// real XPath engine rejects. DocType is prolog-only (at most one per
/// document) and rarely a deliberate yank target, so an explicit "not
/// addressable" error is the honest answer, not a best-effort guess.
///
/// One-off convenience (builds a throwaway `PathCache`); repeated callers
/// should keep a `PathCache` instead.
#[allow(dead_code)] // the app uses `PathCache::build_xpath`; kept for one-off callers
pub fn build_xpath(doc: &Document, index: Index) -> Result<String, String> {
    PathCache::default().build_xpath(doc, index)
}

/// The "literal path" header toggle's other mode: a plain breadcrumb of
/// tag names (`catalog/book/title`, no leading `/`) rather than
/// `build_xpath`'s formal `/catalog[1]/book[1]/title[1]` — the
/// `[n]` position predicate is included only where it actually
/// disambiguates (an element with a same-tag sibling), which is exactly
/// the "shortest unambiguous path" simplification `build_xpath`'s own
/// doc comment flags as deliberately *not* doing (that function always
/// includes `[n]`, matching what XPath tooling like browser devtools'
/// "Copy XPath" produces — this one optimizes for "quick to read at a
/// glance," not "matches a known convention").
///
/// Never fails, unlike `build_xpath` — a `DocType` row just gets a plain
/// "DOCTYPE" segment, since this was never claiming to be valid XPath
/// syntax in the first place.
#[allow(dead_code)] // the header uses `PathCache::build_path`; kept for one-off callers
pub fn build_literal_path(doc: &Document, index: Index) -> String {
    PathCache::default().build_literal_path(doc, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Source};
    use crate::xmlparser;

    fn doc(xml: &str) -> Document {
        let flat = xmlparser::parse(xml.as_bytes()).unwrap();
        Document {
            source: Source::Owned(xml.as_bytes().to_vec()),
            flat,
        }
    }

    #[test]
    fn test_simple_path() {
        let d = doc("<root><a><b>1</b></a></root>");
        // 0 root-open, 1 a-open, 2 b-open, 3 text, 4 b-close, 5 a-close, 6 root-close
        assert_eq!(build_xpath(&d, 2).unwrap(), "/root[1]/a[1]/b[1]");
    }

    #[test]
    fn test_same_tag_siblings() {
        let d = doc("<root><item>1</item><item>2</item><item>3</item></root>");
        // items at rows 1, 4, 7 (each: open, text, close = 3 rows)
        assert_eq!(build_xpath(&d, 1).unwrap(), "/root[1]/item[1]");
        assert_eq!(build_xpath(&d, 4).unwrap(), "/root[1]/item[2]");
        assert_eq!(build_xpath(&d, 7).unwrap(), "/root[1]/item[3]");
    }

    #[test]
    fn test_text_node_path() {
        let d = doc("<root><a>hello</a></root>");
        // 0 root-open, 1 a-open, 2 text, 3 a-close, 4 root-close
        assert_eq!(build_xpath(&d, 2).unwrap(), "/root[1]/a[1]/text()");
    }

    #[test]
    fn test_closing_row_normalizes_to_opening() {
        let d = doc("<root><a>1</a></root>");
        // 0 root-open 1 a-open 2 text 3 a-close 4 root-close
        assert_eq!(build_xpath(&d, 3).unwrap(), build_xpath(&d, 1).unwrap());
    }

    #[test]
    fn test_doctype_has_no_xpath() {
        let d = doc("<!DOCTYPE html><root/>");
        // 0 doctype, 1 root (empty element)
        assert!(build_xpath(&d, 0).is_err());
    }

    // --- build_literal_path ---

    #[test]
    fn test_literal_path_omits_index_when_unique() {
        let d = doc("<root><a><b>1</b></a></root>");
        // 0 root-open, 1 a-open, 2 b-open, 3 text, 4 b-close, 5 a-close, 6 root-close
        assert_eq!(build_literal_path(&d, 2), "root/a/b");
    }

    #[test]
    fn test_literal_path_includes_index_only_for_same_tag_siblings() {
        let d = doc("<root><item>1</item><item>2</item><a/></root>");
        // 0 root-open, 1 item-open, 2 text, 3 item-close,
        // 4 item-open, 5 text, 6 item-close, 7 a-empty, 8 root-close
        assert_eq!(build_literal_path(&d, 1), "root/item[1]");
        assert_eq!(build_literal_path(&d, 4), "root/item[2]");
        // `a` has no same-tag sibling, so no [n] even though it's not
        // the first child overall (unlike build_xpath, which would still
        // say a[1] here).
        assert_eq!(build_literal_path(&d, 7), "root/a");
    }

    #[test]
    fn test_literal_path_text_node() {
        let d = doc("<root><a>hello</a></root>");
        assert_eq!(build_literal_path(&d, 2), "root/a/text");
    }

    #[test]
    fn test_literal_path_doctype_never_errors() {
        let d = doc("<!DOCTYPE html><root/>");
        assert_eq!(build_literal_path(&d, 0), "DOCTYPE");
    }

    #[test]
    fn test_cache_matches_fresh_builds_across_many_lookups() {
        // One PathCache reused for every row (as the header does) must
        // agree with a fresh build for each, in both styles.
        let d = doc("<r><a/><b><a/><a>t</a><c/></b><a/><!--x--></r>");
        let mut cache = PathCache::default();
        for i in 0..d.flat.len() as Index {
            for style in [PathStyle::XPath, PathStyle::Literal] {
                assert_eq!(
                    cache.build_path(&d, i, style),
                    PathCache::default().build_path(&d, i, style),
                    "row {i}, {style:?}"
                );
            }
        }
        // 0 r, 1 a, 2 b, 3 a, 4 a, 5 text, 6 /a, 7 c, 8 /b, 9 a, 10 comment, 11 /r
        assert_eq!(cache.build_path(&d, 9, PathStyle::XPath), "/r[1]/a[2]");
        assert_eq!(cache.build_path(&d, 4, PathStyle::Literal), "r/b/a[2]");
        assert_eq!(cache.build_path(&d, 7, PathStyle::Literal), "r/b/c");
    }

    #[test]
    fn test_literal_path_closing_row_normalizes_to_opening() {
        let d = doc("<root><a>1</a></root>");
        assert_eq!(build_literal_path(&d, 3), build_literal_path(&d, 1));
    }
}
