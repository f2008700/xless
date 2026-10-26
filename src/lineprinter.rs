// Renders a single row's text content (no indentation — the caller adds
// that based on `row.depth`) and the whole-document pretty-print used for
// pipe/filter mode (see README.md's Vim integration section §1's `:r !xless %` /
// `xless file.xml | ...` recipes).
//
// Two kinds of output, deliberately kept separate:
//
// - `render_row` — the interactive view, one terminal line per row, so it
//   flattens embedded newlines/tabs (`sanitize_for_line`) and honors
//   Line/Compact mode (README.md's Architecture section §5) and collapse
//   state.
// - The full-fidelity printers (`pretty_printed`, `pretty_printed_subtree`,
//   `one_line_subtree`) — pipe mode and yank. These must never change
//   content: only inter-node whitespace is re-laid-out, and
//   `xml:space="preserve"` scopes are copied verbatim.
//
// v1 simplification vs. jless's src/lineprinter.rs (1984 lines): no
// horizontal truncation/scrolling for lines wider than the terminal
// (jless's truncatedstrview.rs, ~1125 lines, deliberately not ported yet —
// see README.md's Roadmap section). screenwriter.rs hard-truncates long
// lines at the right edge instead.

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

/// The quote character `attr`'s value was written with in the source.
/// `Attr.value` excludes the quotes, so it's the byte just before the
/// value. Re-emitting the *original* quote matters for correctness, not
/// just fidelity: `a='say "hi"'` re-quoted with `"` would be malformed.
pub fn attr_quote(doc: &Document, attr: &Attr) -> char {
    doc.source[attr.value.start as usize - 1] as char
}

/// ` name="value"` for each attribute. `one_line` collapses embedded
/// newlines/tabs for the interactive one-row-per-line view; the
/// full-fidelity renderers pass `false` and keep the value byte-for-byte.
fn format_attrs(doc: &Document, attrs: &[Attr], one_line: bool) -> String {
    let mut out = String::new();
    for attr in attrs {
        let q = attr_quote(doc, attr);
        let value = doc.text(attr.value.clone());
        out.push(' ');
        out.push_str(doc.text(attr.name.clone()));
        out.push('=');
        out.push(q);
        if one_line {
            out.push_str(&sanitize_for_line(value));
        } else {
            out.push_str(value);
        }
        out.push(q);
    }
    out
}

/// Whether `index` sits in an `xml:space="preserve"` scope — the nearest
/// `xml:space` attribute on it or an ancestor says `preserve`.
fn preserves_space(doc: &Document, mut index: Index) -> bool {
    loop {
        if let Some(v) = own_xml_space(doc, index) {
            return v;
        }
        match doc.flat[index].parent() {
            OptionIndex::Index(p) => index = p,
            OptionIndex::Nil => return false,
        }
    }
}

/// `Some(true/false)` if this row is an element carrying its own
/// `xml:space="preserve"`/`"default"`, else `None`.
fn own_xml_space(doc: &Document, index: Index) -> Option<bool> {
    let index = if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    };
    doc.flat.attrs_of(index).iter().find_map(|a| {
        if doc.text(a.name.clone()) != "xml:space" {
            return None;
        }
        match doc.text(a.value.clone()) {
            "preserve" => Some(true),
            "default" => Some(false),
            _ => None,
        }
    })
}

/// Renders `index`'s own line, honoring collapsed state (a collapsed
/// element renders as a single-line `<tag attrs>…</tag>` preview,
/// computed in O(1) regardless of how large the collapsed subtree is —
/// this matters at the 100-200MB scale just as much as it does for
/// interactivity, see README.md's Architecture section §8) and `mode`
/// (README.md's Architecture section §5, viewer.rs's module doc comment): in Compact
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
            let attrs = format_attrs(doc, doc.flat.attrs_of(index), true);
            format!("<{tag}{attrs}/>")
        }
        Value::OpenElement {
            collapsed,
            close_index,
            ..
        } => {
            let attrs = format_attrs(doc, doc.flat.attrs_of(index), true);
            if *collapsed {
                format!("<{tag}{attrs}>\u{2026}</{tag}>")
            } else if mode == Mode::Compact && *close_index == index + 1 {
                format!("<{tag}{attrs}/>")
            } else {
                format!("<{tag}{attrs}>")
            }
        }
        Value::CloseElement { .. } => format!("</{tag}>"),
    }
}

/// How `write_full_fidelity_row` emits a `Text` row's content.
#[derive(Copy, Clone)]
enum TextForm {
    /// Byte-for-byte, as in the source (one-line yank).
    Raw,
    /// Leading/trailing whitespace trimmed — for the re-indenting
    /// pretty-printers, where that whitespace is the *source's* layout
    /// (the same insignificant formatting whitespace the parser already
    /// drops when it makes up a whole node). Interior text, including
    /// newlines and tabs, is kept exactly.
    Trimmed,
}

/// Appends row `i`'s full-fidelity markup (tag/attrs/content, no
/// indentation, no trailing newline) to `out` — always complete, never
/// mode- or collapse-state-dependent, and never rewriting content: unlike
/// `render_row`, embedded newlines/tabs in text, CDATA, comments, PIs and
/// attribute values are kept as-is, and each attribute keeps its original
/// quote character. (An earlier version reused `render_row`'s
/// one-line sanitizing here, so `xless f.xml > out.xml` silently turned
/// multi-line CDATA into one line and tabs into spaces.)
fn write_full_fidelity_row(doc: &Document, i: Index, out: &mut String, text: TextForm) {
    let row = &doc.flat[i];
    let tag = doc.text(row.range.clone());
    let content = doc.text(row.range.clone());
    match &row.value {
        Value::Text => match text {
            TextForm::Raw => out.push_str(content),
            TextForm::Trimmed => out.push_str(content.trim()),
        },
        Value::CData => {
            out.push_str("<![CDATA[");
            out.push_str(content);
            out.push_str("]]>");
        }
        Value::Comment => {
            out.push_str("<!--");
            out.push_str(content);
            out.push_str("-->");
        }
        Value::ProcessingInstruction => {
            out.push_str("<?");
            out.push_str(content);
            out.push_str("?>");
        }
        Value::DocType => {
            out.push_str("<!DOCTYPE");
            out.push_str(content);
            out.push('>');
        }
        Value::EmptyElement { .. } => {
            out.push('<');
            out.push_str(tag);
            out.push_str(&format_attrs(doc, doc.flat.attrs_of(i), false));
            out.push_str("/>");
        }
        Value::OpenElement { .. } => {
            out.push('<');
            out.push_str(tag);
            out.push_str(&format_attrs(doc, doc.flat.attrs_of(i), false));
            out.push('>');
        }
        Value::CloseElement { .. } => {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

/// Pretty-prints rows `start..=end` (a whole subtree, or the whole
/// document), indented relative to `base_depth`. Anything in an
/// `xml:space="preserve"` scope is copied verbatim from the source on a
/// single (indented) line instead of being re-indented, since adding
/// newlines/indentation inside it would change its declared-significant
/// whitespace.
fn write_pretty_rows(
    doc: &Document,
    start: Index,
    end: Index,
    base_depth: u32,
    out: &mut String,
    flush: &mut dyn FnMut(&mut String) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut i = start;
    loop {
        let row = &doc.flat[i];
        for _ in 0..(row.depth - base_depth) {
            out.push_str("  ");
        }
        let verbatim = if i == start {
            preserves_space(doc, i)
        } else {
            own_xml_space(doc, i) == Some(true)
        };
        if verbatim {
            out.push_str(doc.text_range(crate::edit::node_full_span(doc, i)));
            if let Value::OpenElement { close_index, .. } = row.value {
                i = close_index;
            }
        } else {
            write_full_fidelity_row(doc, i, out, TextForm::Trimmed);
        }
        out.push('\n');
        if out.len() >= FLUSH_THRESHOLD {
            flush(out)?;
        }
        if i >= end {
            break;
        }
        i += 1;
    }
    Ok(())
}

/// Full, always-expanded pretty-print of the whole document — independent
/// of any interactive collapse state (which is exactly why it takes a
/// `Document`, not a `Viewer`: there's no cursor/collapse state involved).
/// Used for pipe/filter mode. Mirrors jless's `FlatJson::pretty_printed()`
/// in spirit (~/git/jless/src/flatjson.rs) — always-expanded output,
/// independent of any interactive view state — but note that jless's
/// version *is* the parser's canonical output (rows point into it),
/// whereas here it's purely a rendering pass over rows that point at the
/// original source (see README.md's Architecture section §8.2 for why).
#[cfg(test)] // pipe mode streams via `write_pretty_printed`; tests compare strings
pub fn pretty_printed(doc: &Document) -> String {
    let mut out = String::new();
    write_pretty_document(doc, &mut out, &mut |_| Ok(())).expect("no-op flush can't fail");
    out
}

/// Streaming version of `pretty_printed` for pipe mode: writes to `w` in
/// ~64KB chunks instead of building the whole (larger-than-the-input)
/// output in memory first — at 150MB that buffer alone was several
/// hundred MB of peak memory.
pub fn write_pretty_printed<W: std::io::Write>(doc: &Document, w: &mut W) -> std::io::Result<()> {
    let mut out = String::with_capacity(FLUSH_THRESHOLD * 2);
    let mut flush = |buf: &mut String| -> std::io::Result<()> {
        w.write_all(buf.as_bytes())?;
        buf.clear();
        Ok(())
    };
    write_pretty_document(doc, &mut out, &mut flush)?;
    flush(&mut out)?;
    w.flush()
}

const FLUSH_THRESHOLD: usize = 64 * 1024;

fn write_pretty_document(
    doc: &Document,
    out: &mut String,
    flush: &mut dyn FnMut(&mut String) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut i: Index = 0;
    while (i as usize) < doc.flat.len() {
        // Top-level nodes (prolog, root, epilog) one at a time, so each
        // gets its own xml:space check from scratch.
        let (start, end) = subtree_row_range(doc, i);
        write_pretty_rows(doc, start, end, 0, out, flush)?;
        i = end + 1;
    }
    Ok(())
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
    let mut out = String::new();
    write_pretty_rows(
        doc,
        start,
        end,
        doc.flat[start].depth,
        &mut out,
        &mut |_| Ok(()),
    )
    .expect("no-op flush can't fail");
    out
}

/// Yank target: the subtree rooted at `index`, minified onto one line
/// (indentation/newlines between nodes stripped; content kept exactly) —
/// the XML analog of jless's `ContentTarget::OneLineValue`.
pub fn one_line_subtree(doc: &Document, index: Index) -> String {
    let (start, end) = subtree_row_range(doc, index);
    let mut out = String::new();
    let mut i = start;
    loop {
        write_full_fidelity_row(doc, i, &mut out, TextForm::Raw);
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
    fn test_pretty_printed_keeps_content_exactly() {
        // Regression test: pipe mode used to flatten newlines/tabs to
        // spaces and re-quote every attribute with `"`.
        let d = doc("<r a='say \"hi\"'><c><![CDATA[line1\nline2]]></c><t>a\tb</t><!--x\ny--></r>");
        assert_eq!(
            pretty_printed(&d),
            "<r a='say \"hi\"'>\n  <c>\n    <![CDATA[line1\nline2]]>\n  </c>\n  <t>\n    a\tb\n  </t>\n  <!--x\ny-->\n</r>\n"
        );
        // And the output is still well-formed XML.
        assert!(xmlparser::parse(pretty_printed(&d).as_bytes()).is_ok());
    }

    #[test]
    fn test_pretty_printed_trims_layout_whitespace_around_text() {
        let d = doc("<r>\n  <a>\n    hello world\n  </a>\n</r>");
        assert_eq!(
            pretty_printed(&d),
            "<r>\n  <a>\n    hello world\n  </a>\n</r>\n"
        );
    }

    #[test]
    fn test_pretty_printed_copies_xml_space_preserve_verbatim() {
        let d = doc("<r><p xml:space=\"preserve\">  <q> x </q>\n</p><s> y </s></r>");
        assert_eq!(
            pretty_printed(&d),
            "<r>\n  <p xml:space=\"preserve\">  <q> x </q>\n</p>\n  <s>\n    y\n  </s>\n</r>\n"
        );
    }

    #[test]
    fn test_subtree_yank_inside_preserve_scope_is_verbatim() {
        let d = doc("<r xml:space=\"preserve\"><q> x </q></r>");
        // 0 r, 1 q, 2 text, 3 /q, 4 /r
        assert_eq!(pretty_printed_subtree(&d, 1), "<q> x </q>\n");
    }

    #[test]
    fn test_render_row_keeps_original_quote() {
        let d = doc("<r a='1'/>");
        assert_eq!(render_row(&d, 0, Mode::Compact), "<r a='1'/>");
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
