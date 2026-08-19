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
}
