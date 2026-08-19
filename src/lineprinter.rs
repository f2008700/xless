// Renders a single row's text content (no indentation — the caller adds
// that based on `row.depth`) and the whole-document pretty-print used for
// pipe/filter mode (see docs/VIM_INTEGRATION.md §1's `:r !xless %` /
// `xless file.xml | ...` recipes).
//
// v1 simplification vs. jless's src/lineprinter.rs (1984 lines): no
// Line-vs-Compact mode distinction yet (docs/ARCHITECTURE.md §5's mode
// table — always-shown attributes/close-tags, single-line collapsed
// previews) and no horizontal truncation/scrolling for lines wider than
// the terminal (jless's truncatedstrview.rs, ~1125 lines, deliberately
// not ported yet — PLAN.md flags this). Long lines currently just run off
// the right edge of the terminal. Both are natural fast-follows once the
// core viewer is proven; deferring them kept the first working version
// achievable in one pass.

use crate::document::Document;
use crate::flatxml::{Attr, Index, OptionIndex, Value};
use crate::viewer::Mode;

/// Collapses embedded newlines/tabs/carriage-returns to spaces so any
/// text/comment/CData/PI content — which can legitimately contain them —
/// still renders as exactly one terminal line, preserving the
/// one-row-per-line invariant the whole viewer depends on.
fn sanitize_for_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn format_attrs(doc: &Document, attrs: &[Attr]) -> String {
    let mut out = String::new();
    for attr in attrs {
        out.push(' ');
        out.push_str(doc.text(attr.name.clone()));
        out.push_str("=\"");
        out.push_str(&sanitize_for_line(doc.text(attr.value.clone())));
        out.push('"');
    }
    out
}

/// Renders `index`'s own line, honoring collapsed state (a collapsed
/// element renders as a single-line `<tag attrs>…</tag>` preview,
/// computed in O(1) regardless of how large the collapsed subtree is —
/// this matters at the 100-200MB scale just as much as it does for
/// interactivity, see docs/ARCHITECTURE.md §8) and `mode`
/// (docs/ARCHITECTURE.md §5, viewer.rs's module doc comment): in Compact
/// mode, an expanded element with no children renders as a self-closing
/// `<tag/>` — its (never independently visible, in Compact mode) closing
/// row is simply never asked to render. In Line mode it renders as a bare
/// `<tag>`, and the separately-visible `CloseElement` row supplies the
/// `</tag>` on its own line.
pub fn render_row(doc: &Document, index: Index, mode: Mode) -> String {
    let row = &doc.flat[index];
    let tag = doc.text(row.range.clone());

    match &row.value {
        Value::Text => sanitize_for_line(doc.text(row.range.clone())),
        Value::CData => format!(
            "<![CDATA[{}]]>",
            sanitize_for_line(doc.text(row.range.clone()))
        ),
        Value::Comment => format!("<!--{}-->", sanitize_for_line(doc.text(row.range.clone()))),
        Value::ProcessingInstruction => {
            format!("<?{}?>", sanitize_for_line(doc.text(row.range.clone())))
        }
        Value::DocType => format!(
            "<!DOCTYPE{}>",
            sanitize_for_line(doc.text(row.range.clone()))
        ),
        Value::EmptyElement { .. } => {
            let attrs = format_attrs(doc, doc.flat.attrs_of(index));
            format!("<{tag}{attrs}/>")
        }
        Value::OpenElement {
            collapsed,
            first_child,
            ..
        } => {
            let attrs = format_attrs(doc, doc.flat.attrs_of(index));
            if *collapsed {
                format!("<{tag}{attrs}>\u{2026}</{tag}>")
            } else if mode == Mode::Compact && matches!(first_child, OptionIndex::Nil) {
                format!("<{tag}{attrs}/>")
            } else {
                format!("<{tag}{attrs}>")
            }
        }
        Value::CloseElement { .. } => format!("</{tag}>"),
    }
}

/// Appends row `i`'s full-fidelity markup (tag/attrs/content, no
/// indentation, no trailing newline) to `out` — always complete, never
/// mode- or collapse-state-dependent. Shared by `pretty_printed` (whole
/// document) and `pretty_printed_subtree`/yank (one node).
fn write_full_fidelity_row(doc: &Document, i: Index, out: &mut String) {
    let row = &doc.flat[i];
    let tag = doc.text(row.range.clone());
    match &row.value {
        Value::Text => out.push_str(&sanitize_for_line(doc.text(row.range.clone()))),
        Value::CData => {
            out.push_str("<![CDATA[");
            out.push_str(&sanitize_for_line(doc.text(row.range.clone())));
            out.push_str("]]>");
        }
        Value::Comment => {
            out.push_str("<!--");
            out.push_str(&sanitize_for_line(doc.text(row.range.clone())));
            out.push_str("-->");
        }
        Value::ProcessingInstruction => {
            out.push_str("<?");
            out.push_str(&sanitize_for_line(doc.text(row.range.clone())));
            out.push_str("?>");
        }
        Value::DocType => {
            out.push_str("<!DOCTYPE");
            out.push_str(&sanitize_for_line(doc.text(row.range.clone())));
            out.push('>');
        }
        Value::EmptyElement { .. } => {
            out.push('<');
            out.push_str(tag);
            out.push_str(&format_attrs(doc, doc.flat.attrs_of(i)));
            out.push_str("/>");
        }
        Value::OpenElement { .. } => {
            out.push('<');
            out.push_str(tag);
            out.push_str(&format_attrs(doc, doc.flat.attrs_of(i)));
            out.push('>');
        }
        Value::CloseElement { .. } => {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

/// Full, always-expanded pretty-print of the whole document — independent
/// of any interactive collapse state (which is exactly why it takes a
/// `Document`, not a `Viewer`: there's no cursor/collapse state involved).
/// Used for pipe/filter mode. Mirrors jless's `FlatJson::pretty_printed()`
/// in spirit (~/git/jless/src/flatjson.rs) — always-expanded output,
/// independent of any interactive view state — but note that jless's
/// version *is* the parser's canonical output (rows point into it),
/// whereas here it's purely a rendering pass over rows that point at the
/// original source (see docs/ARCHITECTURE.md §8.2 for why).
pub fn pretty_printed(doc: &Document) -> String {
    let mut out = String::new();
    for i in 0..doc.flat.len() as Index {
        let row = &doc.flat[i];
        for _ in 0..row.depth {
            out.push_str("  ");
        }
        write_full_fidelity_row(doc, i, &mut out);
        out.push('\n');
    }
    out
}

/// The `(start, end)` row range (inclusive) of the node at `index`'s
/// subtree — a single row for a primitive, or open..=close for a
/// container. Normalizes a closing-tag row to its opening row first.
fn subtree_row_range(doc: &Document, index: Index) -> (Index, Index) {
    let index = if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    };
    match &doc.flat[index].value {
        Value::OpenElement { close_index, .. } => (index, *close_index),
        _ => (index, index),
    }
}

/// Yank target: the subtree rooted at `index`, pretty-printed at its own
/// depth (re-indented from 0, not from wherever it sits in the full
/// document).
pub fn pretty_printed_subtree(doc: &Document, index: Index) -> String {
    let (start, end) = subtree_row_range(doc, index);
    let base_depth = doc.flat[start].depth;
    let mut out = String::new();
    let mut i = start;
    loop {
        let row = &doc.flat[i];
        for _ in 0..(row.depth - base_depth) {
            out.push_str("  ");
        }
        write_full_fidelity_row(doc, i, &mut out);
        out.push('\n');
        if i == end {
            break;
        }
        i += 1;
    }
    out
}

/// Yank target: the subtree rooted at `index`, minified onto one line
/// (indentation/newlines stripped) — the XML analog of jless's
/// `ContentTarget::OneLineValue`.
pub fn one_line_subtree(doc: &Document, index: Index) -> String {
    let (start, end) = subtree_row_range(doc, index);
    let mut out = String::new();
    let mut i = start;
    loop {
        write_full_fidelity_row(doc, i, &mut out);
        if i == end {
            break;
        }
        i += 1;
    }
    out
}

/// Yank target: concatenated descendant text/CData content (DOM
/// `.textContent` semantics — no separators inserted), or the row's own
/// content directly for a Text/CData/Comment row.
pub fn text_content(doc: &Document, index: Index) -> String {
    let index = if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    };
    match &doc.flat[index].value {
        Value::Text | Value::CData => return doc.row_text(index).to_string(),
        Value::Comment => return doc.row_text(index).to_string(),
        _ => {}
    }
    let (start, end) = subtree_row_range(doc, index);
    let mut out = String::new();
    for i in start..=end {
        if matches!(doc.flat[i].value, Value::Text | Value::CData) {
            out.push_str(doc.row_text(i));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Source;
    use crate::xmlparser;

    fn doc(xml: &str) -> Document {
        let flat = xmlparser::parse(xml.as_bytes()).unwrap();
        Document {
            source: Source::Owned(xml.as_bytes().to_vec()),
            flat,
        }
    }

    #[test]
    fn test_compact_mode_hides_empty_close_row() {
        let d = doc("<root><a></a></root>");
        // 0 root-open, 1 a-open, 2 a-close, 3 root-close
        assert_eq!(render_row(&d, 1, Mode::Compact), "<a/>");
        assert_eq!(render_row(&d, 1, Mode::Line), "<a>");
        assert_eq!(render_row(&d, 2, Mode::Line), "</a>");
    }

    #[test]
    fn test_collapsed_preview_same_both_modes() {
        let mut d = doc("<root><a><b>1</b></a></root>");
        d.flat.collapse(1);
        assert_eq!(render_row(&d, 1, Mode::Compact), "<a>\u{2026}</a>");
        assert_eq!(render_row(&d, 1, Mode::Line), "<a>\u{2026}</a>");
    }

    #[test]
    fn test_pretty_printed_subtree() {
        let d = doc("<root><a><b>1</b></a><c>2</c></root>");
        // a is at rows 1..=4 (a-open, b-open, text, b-close, a-close = 5 rows, index 1..=5)
        let out = pretty_printed_subtree(&d, 1);
        assert_eq!(out, "<a>\n  <b>\n    1\n  </b>\n</a>\n");
    }

    #[test]
    fn test_one_line_subtree() {
        let d = doc("<root><a><b>1</b></a></root>");
        assert_eq!(one_line_subtree(&d, 1), "<a><b>1</b></a>");
    }

    #[test]
    fn test_text_content_concatenates_descendants() {
        let d = doc("<p>Hello <b>world</b>!</p>");
        assert_eq!(text_content(&d, 0), "Hello world!");
    }
}
