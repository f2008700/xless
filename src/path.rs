// Builds an XPath-flavored path to a row — the XML analog of jless's
// `FlatJson::build_path_to_node` (~/git/jless/src/flatjson.rs), which
// builds jq-style paths (`.foo[3].bar`). See docs/ARCHITECTURE.md §7.1:
// XPath's sibling-position convention is 1-based and counts only
// same-tag siblings (`item[2]` means "the 2nd `<item>` among its
// siblings," not "the 2nd child overall"), which is different enough
// from jless's all-siblings `index_in_parent` that this is computed
// fresh here by walking `prev_sibling` and comparing tag text, rather
// than reusing `Row.index_in_parent`.
//
// v1 simplification: this always includes the `[n]` predicate, even when
// an element has no same-tag siblings (where strict "shortest unambiguous
// path" XPath tooling would omit it). Always-included is simpler, still
// unambiguous, and matches what e.g. browser devtools' "Copy XPath"
// produces — a convenience path for humans/copy-paste, not a spec-minimal
// XPath compiler. Attributes aren't reachable (no `@name` segments)
// because attributes aren't individually focusable yet (ARCHITECTURE.md
// §7.2/§7.3).

use crate::document::Document;
use crate::flatxml::{Index, OptionIndex, Value};

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
pub fn build_path(doc: &Document, index: Index, style: PathStyle) -> String {
    match style {
        PathStyle::XPath => build_xpath(doc, index).unwrap_or_else(|e| format!("({e})")),
        PathStyle::Literal => build_literal_path(doc, index),
    }
}

fn element_tag(doc: &Document, index: Index) -> Option<&str> {
    match &doc.flat[index].value {
        Value::OpenElement { .. } | Value::EmptyElement { .. } => Some(doc.row_text(index)),
        _ => None,
    }
}

/// 1-based position of `index` among its *same-tag* siblings.
fn same_tag_position(doc: &Document, index: Index) -> usize {
    let Some(tag) = element_tag(doc, index) else {
        return 1;
    };
    let mut count = 1usize;
    let mut prev = doc.flat[index].prev_sibling;
    while let OptionIndex::Index(p) = prev {
        if element_tag(doc, p) == Some(tag) {
            count += 1;
        }
        prev = doc.flat[p].prev_sibling;
    }
    count
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
pub fn build_xpath(doc: &Document, index: Index) -> Result<String, String> {
    let index = if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    };

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

    if let Some(node_test) = leaf_node_test {
        segments.push(node_test.to_string());
        if let OptionIndex::Index(parent) = doc.flat[index].parent {
            build_element_path(doc, parent, &mut segments);
        }
    } else {
        build_element_path(doc, index, &mut segments);
    }

    segments.reverse();
    Ok(format!("/{}", segments.join("/")))
}

fn build_element_path(doc: &Document, index: Index, segments: &mut Vec<String>) {
    let tag = element_tag(doc, index).unwrap_or("?");
    let pos = same_tag_position(doc, index);
    segments.push(format!("{tag}[{pos}]"));
    if let OptionIndex::Index(parent) = doc.flat[index].parent {
        build_element_path(doc, parent, segments);
    }
}

/// True if no sibling (in either direction) shares `index`'s tag — i.e.
/// `same_tag_position` would always be 1 for it, so a `[1]` predicate
/// would carry no information. Checks both directions, not just "nothing
/// before it," since a same-tag sibling *after* it would make position 1
/// just as ambiguous as one before it would.
fn is_unique_among_siblings(doc: &Document, index: Index) -> bool {
    let Some(tag) = element_tag(doc, index) else {
        return true;
    };
    let mut prev = doc.flat[index].prev_sibling;
    while let OptionIndex::Index(p) = prev {
        if element_tag(doc, p) == Some(tag) {
            return false;
        }
        prev = doc.flat[p].prev_sibling;
    }
    let mut next = doc.flat[index].next_sibling;
    while let OptionIndex::Index(n) = next {
        if element_tag(doc, n) == Some(tag) {
            return false;
        }
        next = doc.flat[n].next_sibling;
    }
    true
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
pub fn build_literal_path(doc: &Document, index: Index) -> String {
    let index = if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    };

    let leaf_label = match &doc.flat[index].value {
        Value::Text => Some("text"),
        Value::CData => Some("CDATA"),
        Value::Comment => Some("comment"),
        Value::ProcessingInstruction => Some("PI"),
        Value::DocType => Some("DOCTYPE"),
        _ => None,
    };

    let mut segments: Vec<String> = Vec::new();

    if let Some(label) = leaf_label {
        segments.push(label.to_string());
        if let OptionIndex::Index(parent) = doc.flat[index].parent {
            build_literal_element_path(doc, parent, &mut segments);
        }
    } else {
        build_literal_element_path(doc, index, &mut segments);
    }

    segments.reverse();
    segments.join("/")
}

fn build_literal_element_path(doc: &Document, index: Index, segments: &mut Vec<String>) {
    let tag = element_tag(doc, index).unwrap_or("?");
    if is_unique_among_siblings(doc, index) {
        segments.push(tag.to_string());
    } else {
        segments.push(format!("{tag}[{}]", same_tag_position(doc, index)));
    }
    if let OptionIndex::Index(parent) = doc.flat[index].parent {
        build_literal_element_path(doc, parent, segments);
    }
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
    fn test_literal_path_closing_row_normalizes_to_opening() {
        let d = doc("<root><a>1</a></root>");
        assert_eq!(build_literal_path(&d, 3), build_literal_path(&d, 1));
    }
}
