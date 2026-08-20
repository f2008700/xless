// Builds a `FlatXml` from raw XML bytes using `quick-xml` as a pull-parser
// (mirrors how jless's `jsonparser.rs` drives its `logos` lexer by hand —
// see ~/git/jless/src/jsonparser.rs).
//
// Key departure from jless's parser (see README.md's Architecture section §4/§8): we
// do NOT re-render a canonical pretty-printed string and point rows into
// that. `quick-xml`'s `Reader::from_str` gives zero-copy events borrowing
// directly from the input, so every `Row.range` is computed by pointer
// arithmetic straight into the *original* source bytes. This is both
// simpler (no second buffer to build) and the basis of the large-file
// memory strategy: nothing here allocates proportionally to the input
// size except the row/attr vectors themselves.
//
// v1 scope (see README.md's Roadmap section M0): single-pass, everything eagerly parsed
// (no phase-1/phase-2 lazy split yet — that's a fallback to reach for only
// if benchmarking shows it's needed, per ARCHITECTURE.md §8.4). Well-formed
// XML only; a parse error aborts with a message, same as jless does for
// malformed JSON/YAML.

use std::borrow::Cow;
use std::ops::Range;

use quick_xml::events::{BytesEnd, BytesStart, Event};
use quick_xml::reader::Reader;

use crate::flatxml::{Attr, FlatXml, Index, OptionIndex, Row, Value, NIL};

pub fn parse(source: &[u8]) -> Result<FlatXml, String> {
    let text = std::str::from_utf8(source).map_err(|e| format!("invalid UTF-8: {e}"))?;

    let mut parser = Parser {
        base: text.as_ptr(),
        rows: Vec::new(),
        attrs: Vec::new(),
        open_stack: Vec::new(),
        level_last_sibling: vec![OptionIndex::Nil],
        level_child_count: vec![0],
        top_level_elements: 0,
    };

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => parser.open_element(&e)?,
            Ok(Event::Empty(e)) => parser.empty_element(&e)?,
            Ok(Event::End(e)) => parser.close_element(&e)?,
            Ok(Event::Text(e)) => {
                if !e.iter().all(u8::is_ascii_whitespace) {
                    validate_references(&e)?;
                    validate_no_cdata_terminator(&e)?;
                    parser.push_leaf(Value::Text, &e);
                }
                // Whitespace-only text nodes are dropped by default — see
                // ARCHITECTURE.md §3. A --show-whitespace flag to keep
                // them is deferred (PLAN.md notes this as a later flag).
            }
            Ok(Event::CData(e)) => parser.push_leaf(Value::CData, &e),
            Ok(Event::Comment(e)) => parser.push_leaf(Value::Comment, &e),
            Ok(Event::PI(e)) => parser.push_leaf(Value::ProcessingInstruction, &e),
            Ok(Event::Decl(e)) => parser.push_leaf(Value::ProcessingInstruction, &e),
            Ok(Event::DocType(e)) => parser.push_leaf(Value::DocType, &e),
            Err(err) => {
                return Err(format!(
                    "XML parse error at byte {}: {err}",
                    reader.buffer_position()
                ));
            }
        }
    }

    if !parser.open_stack.is_empty() {
        return Err("unexpected end of input: unclosed element(s)".to_string());
    }

    if parser.rows.is_empty() {
        return Err("empty document".to_string());
    }

    if parser.top_level_elements == 0 {
        return Err("not well-formed: document has no root element".to_string());
    }
    if parser.top_level_elements > 1 {
        return Err(format!(
            "not well-formed: document has {} top-level elements, expected exactly 1 root element",
            parser.top_level_elements
        ));
    }

    Ok(FlatXml {
        rows: parser.rows,
        attrs: parser.attrs,
    })
}

struct Parser {
    base: *const u8,
    rows: Vec<Row>,
    attrs: Vec<Attr>,

    // Stack of currently-open element row indices; len() == current depth.
    open_stack: Vec<Index>,
    // One entry per current level (index 0 = top level, further entries
    // pushed/popped alongside open_stack): the last sibling row index
    // registered so far at that level, used to link prev/next_sibling and
    // to detect "this is the first child" (to set the parent's
    // first_child). See flatxml.rs's module doc comment for why close
    // rows are *not* registered here (they occupy the same sibling slot
    // as their matching open row from the parent's point of view).
    level_last_sibling: Vec<OptionIndex>,
    level_child_count: Vec<u32>,

    // Count of elements (Start/Empty) seen at depth 0 — must end at
    // exactly 1 for a well-formed document (a single root element; the
    // prolog/epilog can have comments/PIs/doctype alongside it, but only
    // one actual element). Not enforced by quick-xml itself in
    // pull-parser mode (it validates tag nesting, not document-level
    // structure), so this parser has to check it directly — see the
    // `Parser::open_element`/`empty_element` increments and the check in
    // `parse()`. Without this, editing commands like `o`/`O` (insert
    // sibling) or `dd` (delete) could silently produce a multi-root or
    // root-less "document" that the edit safety net's well-formedness
    // check (edit.rs, re-parse-and-validate) would wrongly accept.
    top_level_elements: u32,
}

impl Parser {
    fn depth(&self) -> u32 {
        self.open_stack.len() as u32
    }

    fn parent(&self) -> OptionIndex {
        match self.open_stack.last() {
            Some(&p) => OptionIndex::Index(p),
            None => OptionIndex::Nil,
        }
    }

    /// Byte range of `bytes` within the original source, computed by
    /// pointer arithmetic since quick-xml's slice-reader events borrow
    /// directly from it (no copying). Panics if `bytes` somehow isn't a
    /// sub-slice of the source — which would mean quick-xml handed us an
    /// owned (re-encoded) buffer, which shouldn't happen since we don't
    /// enable the `encoding` feature and always feed it UTF-8 `&str`.
    fn offset(&self, bytes: &[u8]) -> Range<u32> {
        let start = unsafe { bytes.as_ptr().offset_from(self.base) };
        assert!(
            start >= 0,
            "byte slice is not part of the parser's source buffer (unexpected owned Cow?)"
        );
        let start = start as u32;
        start..(start + bytes.len() as u32)
    }

    // clippy suggests collapsing `&Cow<[u8]>` to `&[u8]`, but that's not
    // applicable here: unlike a normal "accept anything deref-able to
    // [u8]" parameter, this function specifically needs to distinguish
    // the Borrowed/Owned variant to catch an unexpected owned buffer (see
    // the panic message below and `offset`'s doc comment).
    #[allow(clippy::ptr_arg)]
    fn cow_offset<'a>(&self, cow: &Cow<'a, [u8]>) -> Range<u32> {
        match cow {
            Cow::Borrowed(b) => self.offset(b),
            Cow::Owned(_) => panic!(
                "unexpected owned attribute/text bytes (encoding feature enabled unexpectedly?)"
            ),
        }
    }

    /// Registers `new_index` (already pushed onto `self.rows`) as the next
    /// sibling at the current level, and — if it's the first row at this
    /// level — records it as its parent's `first_child`.
    fn link_sibling(&mut self, new_index: Index) {
        let level = self.level_last_sibling.len() - 1;
        let prev = self.level_last_sibling[level];

        self.rows[new_index as usize].prev_sibling = prev;
        if let OptionIndex::Index(p) = prev {
            self.rows[p as usize].next_sibling = OptionIndex::Index(new_index);
        }

        self.rows[new_index as usize].index_in_parent = self.level_child_count[level];
        self.level_child_count[level] += 1;
        self.level_last_sibling[level] = OptionIndex::Index(new_index);

        if self.level_child_count[level] == 1 {
            if let Some(&parent) = self.open_stack.last() {
                if let Value::OpenElement { first_child, .. } =
                    &mut self.rows[parent as usize].value
                {
                    *first_child = OptionIndex::Index(new_index);
                }
            }
        }
    }

    fn push_leaf<T: std::ops::Deref<Target = [u8]>>(&mut self, value: Value, event: &T) {
        let range = self.offset(event);
        let new_index = self.rows.len() as Index;
        self.rows.push(Row {
            parent: self.parent(),
            prev_sibling: OptionIndex::Nil,
            next_sibling: OptionIndex::Nil,
            depth: self.depth(),
            index_in_parent: 0,
            range,
            value,
        });
        self.link_sibling(new_index);
    }

    fn parse_attrs(&mut self, e: &BytesStart) -> Result<Range<u32>, String> {
        let start = self.attrs.len() as u32;
        for attr in e.attributes() {
            let attr = attr.map_err(|err| format!("invalid attribute: {err}"))?;
            validate_references(&attr.value)
                .map_err(|msg| format!("invalid attribute value: {msg}"))?;
            let name = self.offset(attr.key.as_ref());
            let value = self.cow_offset(&attr.value);
            self.attrs.push(Attr { name, value });
        }
        let end = self.attrs.len() as u32;
        Ok(start..end)
    }

    fn open_element(&mut self, e: &BytesStart) -> Result<(), String> {
        let range = self.offset(e.name().as_ref());
        let attrs = self.parse_attrs(e)?;

        if self.open_stack.is_empty() {
            self.top_level_elements += 1;
        }

        let new_index = self.rows.len() as Index;
        self.rows.push(Row {
            parent: self.parent(),
            prev_sibling: OptionIndex::Nil,
            next_sibling: OptionIndex::Nil,
            depth: self.depth(),
            index_in_parent: 0,
            range,
            value: Value::OpenElement {
                collapsed: false,
                attrs,
                first_child: OptionIndex::Nil,
                close_index: NIL,
            },
        });
        self.link_sibling(new_index);

        self.open_stack.push(new_index);
        self.level_last_sibling.push(OptionIndex::Nil);
        self.level_child_count.push(0);

        Ok(())
    }

    fn empty_element(&mut self, e: &BytesStart) -> Result<(), String> {
        let range = self.offset(e.name().as_ref());
        let attrs = self.parse_attrs(e)?;

        if self.open_stack.is_empty() {
            self.top_level_elements += 1;
        }

        let new_index = self.rows.len() as Index;
        self.rows.push(Row {
            parent: self.parent(),
            prev_sibling: OptionIndex::Nil,
            next_sibling: OptionIndex::Nil,
            depth: self.depth(),
            index_in_parent: 0,
            range,
            value: Value::EmptyElement { attrs },
        });
        self.link_sibling(new_index);

        Ok(())
    }

    fn close_element(&mut self, e: &BytesEnd) -> Result<(), String> {
        let open_index = self
            .open_stack
            .pop()
            .ok_or_else(|| "unmatched closing tag".to_string())?;
        let last_child = self.level_last_sibling.pop().unwrap();
        self.level_child_count.pop();

        let range = self.offset(e.name().as_ref());
        let parent = self.rows[open_index as usize].parent;
        let depth = self.rows[open_index as usize].depth;
        let index_in_parent = self.rows[open_index as usize].index_in_parent;

        let close_index = self.rows.len() as Index;
        self.rows.push(Row {
            parent,
            prev_sibling: OptionIndex::Nil,
            next_sibling: OptionIndex::Nil,
            depth,
            index_in_parent,
            range,
            value: Value::CloseElement {
                collapsed: false,
                last_child,
                open_index,
            },
        });

        if let Value::OpenElement {
            close_index: ci, ..
        } = &mut self.rows[open_index as usize].value
        {
            *ci = close_index;
        }

        Ok(())
    }
}

/// Well-formedness check XML's grammar requires that quick-xml's raw
/// tokenizer doesn't enforce on its own (confirmed directly: parsing
/// `<root a="x & y">` or `<root>x & y</root>` with quick-xml's default,
/// non-`.unescape()`-ing event reader succeeds with no error — found by
/// review). Per the XML spec's `content`/`AttValue` grammar, every
/// literal `&` in character data or an attribute value must start a
/// `Reference`: `&name;` (an entity reference) or `&#10;`/`&#x1F;` (a
/// character reference) — never appear bare. This only checks that the
/// *syntax* after `&` is legal, not that a named entity is actually
/// defined anywhere (that's a DTD-dependent *validity* constraint,
/// stricter than well-formedness and out of scope — `&whatever;` is
/// well-formed even though only `&amp;`/`&lt;`/`&gt;`/`&apos;`/`&quot;`
/// are predefined without a DTD).
///
/// Deliberately does *not* check for bare `<` — unlike `&`, a literal
/// unescaped `<` can't silently pass through unnoticed: quick-xml's
/// tokenizer has to interpret every `<` as the start of markup at
/// tokenization time, so a stray one either becomes an unintended nested
/// tag (a structural change quick-xml itself handles, correctly or not)
/// or fails to tokenize as valid markup and surfaces as a quick-xml error
/// already — there's no code path where it's silently accepted as if it
/// were plain text, the way a bare `&` is.
fn validate_references(bytes: &[u8]) -> Result<(), String> {
    // Safe: `bytes` is always a sub-slice of the source buffer that
    // `parse()` already validated as UTF-8 in full before tokenizing.
    let s = std::str::from_utf8(bytes).expect("sub-slice of already-UTF-8-validated source");

    let mut search_from = 0usize;
    while let Some(rel) = s[search_from..].find('&') {
        let amp = search_from + rel;
        let rest = &s[amp + 1..];
        let Some(semi_rel) = rest.find(';') else {
            return Err(format!(
                "unescaped '&' at byte {amp} (use '&amp;' for a literal ampersand)"
            ));
        };
        // A bare '&' followed eventually by *some* ';' elsewhere (e.g.
        // "a & b; c") isn't a reference just because there's a semicolon
        // somewhere downstream — reject if the "name" portion contains
        // whitespace or another '&', which a real reference name never
        // does.
        let name = &rest[..semi_rel];
        let is_valid =
            if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
            } else if let Some(dec) = name.strip_prefix('#') {
                !dec.is_empty() && dec.chars().all(|c| c.is_ascii_digit())
            } else {
                is_valid_xml_name(name)
            };
        if !is_valid {
            return Err(format!(
                "unescaped '&' at byte {amp} (use '&amp;' for a literal ampersand)"
            ));
        }
        search_from = amp + 1 + semi_rel + 1;
    }
    Ok(())
}

/// The other well-formedness rule quick-xml's raw tokenizer doesn't
/// enforce (confirmed directly, same way as `validate_references`):
/// XML's `CharData` grammar forbids the literal three-character sequence
/// `]]>` in ordinary text, not just inside a `CDATA` section — precisely
/// to avoid the ambiguity a `CDATA`-splitting edit could otherwise
/// create silently. Without this check, editing a `<![CDATA[...]]>`
/// section's raw content (`i` on a CData row, app.rs) to itself contain
/// `]]>` would "successfully" reparse into a shorter CDATA section
/// followed by a sibling text node holding the rest — a structural
/// change the user never asked for and the reparse-based safety net
/// (edit.rs) wouldn't catch, since quick-xml doesn't reject it either.
fn validate_no_cdata_terminator(bytes: &[u8]) -> Result<(), String> {
    let s = std::str::from_utf8(bytes).expect("sub-slice of already-UTF-8-validated source");
    if let Some(rel) = s.find("]]>") {
        return Err(format!(
            "text content contains ']]>' at byte {rel}, which is not allowed outside a CDATA section"
        ));
    }
    Ok(())
}

/// Simplified XML `Name` check (permissive on the Unicode-letter
/// question via `char::is_alphabetic`/`is_alphanumeric` rather than
/// matching the XML spec's exact `NameStartChar`/`NameChar` production
/// character-for-character) — good enough to distinguish "this looks
/// like an entity name" from "this is clearly not one," which is all
/// `validate_references` needs.
fn is_valid_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == ':' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | ':' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let fj = parse(b"<root><a>1</a><b/></root>").unwrap();
        // 0: root-open, 1: a-open, 2: a-text, 3: a-close, 4: b-empty, 5: root-close
        assert_eq!(fj.len(), 6);
        assert!(fj[0].is_opening_of_container());
        assert!(matches!(fj[4].value, Value::EmptyElement { .. }));
        assert_eq!(fj[5].pair_index(), OptionIndex::Index(0));
    }

    #[test]
    fn test_attributes() {
        let fj = parse(br#"<root a="1" b='two'></root>"#).unwrap();
        let attrs = fj.attrs_of(0);
        assert_eq!(attrs.len(), 2);
    }

    #[test]
    fn test_whitespace_dropped_by_default() {
        let fj = parse(b"<root>\n  <a/>\n</root>").unwrap();
        // root-open, a-empty, root-close: whitespace text nodes dropped.
        assert_eq!(fj.len(), 3);
    }

    #[test]
    fn test_comment_cdata_pi() {
        let fj = parse(b"<?xml version=\"1.0\"?><root><!--hi--><![CDATA[<raw>]]></root>").unwrap();
        assert!(matches!(fj[0].value, Value::ProcessingInstruction));
    }

    #[test]
    fn test_unmatched_tag_is_error() {
        assert!(parse(b"<a><b></a></b>").is_err());
    }

    #[test]
    fn test_mixed_content() {
        let fj = parse(b"<p>Hello <b>world</b>!</p>").unwrap();
        // p-open, text("Hello "), b-open, text("world"), b-close, text("!"), p-close
        assert_eq!(fj.len(), 7);
    }

    #[test]
    fn test_multiple_top_level_elements_is_error() {
        // Two root elements — not well-formed XML. Without this check,
        // editing commands (o/O/dd) could silently produce exactly this
        // and have it accepted by the edit safety net's reparse check.
        let err = parse(b"<a/><b/>").unwrap_err();
        assert!(
            err.contains("top-level elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_multiple_top_level_elements_with_prolog_comments_is_still_error() {
        // Comments/PIs alongside the root are fine (that's the prolog/
        // epilog); a *second element* is not.
        let err = parse(b"<?xml version=\"1.0\"?><a/><!--hi--><b/>").unwrap_err();
        assert!(
            err.contains("top-level elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_no_root_element_is_error() {
        // Only a comment, no element at all.
        let err = parse(b"<!--just a comment-->").unwrap_err();
        assert!(err.contains("no root element"), "unexpected error: {err}");
    }

    #[test]
    fn test_single_root_with_prolog_and_epilog_comments_is_ok() {
        let fj = parse(b"<?xml version=\"1.0\"?><!--before--><root/><!--after-->").unwrap();
        // PI, comment, root (empty element), comment.
        assert_eq!(fj.len(), 4);
    }

    // --- Regression tests: bare '&' must be rejected (found by review) ---

    #[test]
    fn test_bare_ampersand_in_text_is_error() {
        let err = parse(b"<root>x & y</root>").unwrap_err();
        assert!(err.contains("unescaped '&'"), "unexpected error: {err}");
    }

    #[test]
    fn test_bare_ampersand_in_attribute_is_error() {
        let err = parse(br#"<root a="x & y"/>"#).unwrap_err();
        assert!(err.contains("unescaped '&'"), "unexpected error: {err}");
    }

    #[test]
    fn test_named_entity_references_are_ok() {
        let fj = parse(b"<root>Q&amp;A &lt;tag&gt; &apos;s &quot;</root>").unwrap();
        assert_eq!(fj.len(), 3); // root-open, text, root-close
    }

    #[test]
    fn test_custom_named_entity_reference_is_well_formed() {
        // Well-formed even though undefined without a DTD — see
        // validate_references's doc comment on well-formedness vs.
        // validity.
        assert!(parse(b"<root>&some_custom_entity;</root>").is_ok());
    }

    #[test]
    fn test_numeric_character_references_are_ok() {
        assert!(parse(b"<root>&#65;&#x41;</root>").is_ok());
    }

    #[test]
    fn test_ampersand_with_unrelated_later_semicolon_is_still_error() {
        // "a & b; c" — there's a ';' somewhere after the '&', but the
        // text between them ("a & b" minus the leading '&'... actually
        // " b") isn't a legal reference name (contains a space), so this
        // must still be rejected, not accidentally accepted because *a*
        // semicolon exists downstream.
        let err = parse(b"<root>a & b; c</root>").unwrap_err();
        assert!(err.contains("unescaped '&'"), "unexpected error: {err}");
    }

    #[test]
    fn test_bare_ampersand_in_attribute_blob_edit_is_now_rejected() {
        // The scenario the review flagged: app.rs's raw attribute-blob
        // edit path (Kind::Attrs in edit_content) deliberately doesn't
        // auto-escape, relying on this parser's validation to catch a
        // bare '&' typed directly into that field. Simulates the
        // resulting buffer directly.
        let err = parse(br#"<root a="Q&A"/>"#).unwrap_err();
        assert!(err.contains("unescaped '&'"), "unexpected error: {err}");
    }

    #[test]
    fn test_literal_cdata_terminator_in_text_is_error() {
        // Regression test: found by review that quick-xml's raw
        // tokenizer accepts a literal "]]>" in ordinary text, which the
        // XML CharData grammar forbids specifically to avoid this
        // silently splitting a CDATA-content edit into two nodes.
        let err = parse(b"<root>foo]]>bar</root>").unwrap_err();
        assert!(err.contains("]]>"), "unexpected error: {err}");
    }

    #[test]
    fn test_cdata_section_itself_may_contain_the_terminator_text_is_fine() {
        // Sanity check the fix isn't overzealous: a real CDATA section's
        // *own* content never literally contains "]]>" (that's what
        // terminates it), so ordinary CDATA usage is unaffected.
        assert!(parse(b"<root><![CDATA[<not & escaped>]]></root>").is_ok());
    }
}
