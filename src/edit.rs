// Editing: patch-based mutation of the document's byte buffer, full
// re-parse on each committed edit, undo/redo, and save. See
// README.md's Editing model section for the original design and README.md's Architecture section §9 for
// why xless has an editing mode at all (a deliberate divergence from
// jless, which is read-only).
//
// v1 implementation choice, called out explicitly because it's a real
// simplification of EDITING.md §3's design, not an oversight: on every
// committed edit (not every keystroke — typing into an insert-mode field
// only touches an in-memory string until Enter confirms it), this
// re-parses the *entire* document rather than just the smallest enclosing
// subtree. EDITING.md §3's subtree-scoped splice requires renumbering
// every row index throughout the whole document that references a
// position past the edit (parent/sibling/pair indices are absolute Vec
// positions), which is a correctness-sensitive optimization worth doing
// carefully in its own pass, not folded into first getting editing
// working at all. Whole-document re-parse is simple, obviously correct,
// and — since it only runs once per *confirmed* edit, not per keystroke —
// is a perfectly reasonable interactive experience even on a 100-200MB
// file (a ~1-2s pause after pressing Enter to confirm a change, not a
// stall while typing). The subtree-scoped version remains a documented
// follow-up (README.md's Roadmap section M6), not a promise this code makes.
//
// Safety net used uniformly for every edit kind (rename, delete, insert,
// text/attribute edit) instead of EDITING.md §5's original per-keystroke
// auto-escaping design: apply the edit, re-parse, and if that fails (the
// edit broke well-formedness), roll the byte buffer back and report the
// parse error rather than leaving a broken document loaded. This is
// simpler to get right than validating character-by-character as the
// user types, and arguably more predictable — you get a clear error
// naming the problem, not silent auto-escaping you didn't ask for. Text
// *content* edits (not raw attribute-string edits) still auto-escape `<`
// and `&` on the way in, since that field is unambiguously "this
// element's text," not "type raw markup here."

use std::io::Write;
use std::ops::Range;
use std::path::Path;

use crate::document::{Document, Source};
use crate::flatxml::{Index, OptionIndex, Value};
use crate::viewer::Viewer;
use crate::xmlparser;

/// One committed edit: one or more byte-range replacements applied
/// together (e.g. renaming a tag touches both its opening and closing
/// spans), captured with enough information to reverse it.
struct PatchSpan {
    /// Range in the buffer *before* this patch was applied. Every span's
    /// resulting position after applying (and after undoing) has the same
    /// `start` as this original range — see the module test for why:
    /// spans are always applied/unapplied strictly right-to-left, so no
    /// span's start is ever shifted by another span's edit.
    range: Range<usize>,
    removed: Vec<u8>,
    inserted: Vec<u8>,
}

pub struct Patch {
    spans: Vec<PatchSpan>,
    /// Byte offset to try to refocus on after applying/undoing this patch.
    focus_hint: usize,
}

fn apply_spans(bytes: &mut Vec<u8>, spans: &mut [PatchSpan]) {
    for span in spans.iter_mut() {
        span.removed = bytes[span.range.clone()].to_vec();
    }
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(spans[i].range.start));
    for i in order {
        let r = spans[i].range.clone();
        bytes.splice(r, spans[i].inserted.iter().copied());
    }
}

// BUG FIX (found by review, see git history): this used to sort
// descending, the same order as `apply_spans`. That's wrong for undo —
// verified by hand-simulation and by algebra, recorded here since it's
// exactly the kind of thing that looks "obviously symmetric with apply"
// and isn't.
//
// After `apply_spans` runs, a span k's content sits not at its original
// `range.start` but at `range.start + sum(delta_j for every OTHER span j
// with a smaller original start)`, where `delta_j = inserted_j.len() -
// removed_j.len()` — because applying any smaller-offset span shifts
// everything after it, including this span's already-placed content.
//
// Undoing in ASCENDING original-start order exploits that directly:
// by the time we get to span k, every smaller-offset span has *already*
// been undone, each contributing exactly `-delta_j` to span k's current
// position — which cancels the `+delta_j` term above exactly. So span
// k's position at the moment we undo it is simply its original
// `range.start`, unadjusted. (This cancellation is specific to undoing
// smaller-offset spans *first*; processing descending, as the old code
// did, undoes larger-offset spans while smaller ones are still applied,
// leaving their shift uncancelled and splicing the wrong bytes whenever
// `inserted.len() != removed.len()` — e.g. renaming `<a>` to
// `<verylong>` — see `test_undo_multi_span_with_length_change`.)
fn unapply_spans(bytes: &mut Vec<u8>, spans: &[PatchSpan]) {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i].range.start);
    for i in order {
        let start = spans[i].range.start;
        let end = start + spans[i].inserted.len();
        bytes.splice(start..end, spans[i].removed.iter().copied());
    }
}

#[derive(Default)]
pub struct EditHistory {
    undo_stack: Vec<Patch>,
    redo_stack: Vec<Patch>,
    /// `undo_stack.len()` at the moment of the last successful save.
    /// Comparing against this (rather than just "is the undo stack
    /// non-empty") means undo-ing back past a save correctly reports
    /// dirty again (the on-disk file no longer matches what's loaded),
    /// and a plain "any history at all" check wouldn't.
    saved_at: usize,
}

impl EditHistory {
    pub fn is_dirty(&self) -> bool {
        self.undo_stack.len() != self.saved_at
    }

    pub fn mark_saved(&mut self) {
        self.saved_at = self.undo_stack.len();
    }

    fn take_owned_bytes(doc: &mut Document) -> Vec<u8> {
        match std::mem::replace(&mut doc.source, Source::Owned(Vec::new())) {
            Source::Owned(v) => v,
            Source::Mapped(m) => m[..].to_vec(),
        }
    }

    /// Tries to parse `bytes` as the new document. On success, updates
    /// `viewer` (source + flat + refocus near `focus_hint`) and returns
    /// `Ok(())`. On failure, returns `bytes` back *unconsumed* — this is
    /// the piece `commit`/`undo`/`redo` all need to roll back cleanly:
    /// `take_owned_bytes` already replaced `viewer.doc.source` with an
    /// empty placeholder, so on error the caller must explicitly restore
    /// a real buffer there, or `viewer.doc.flat`'s ranges (still pointing
    /// at the old content) would index into nothing on the next render.
    /// (An earlier version of `undo`/`redo` used `?` directly against a
    /// `Result<(), String>`-returning helper and lost `bytes` on error
    /// this way — found by review, fixed by threading the bytes through
    /// the `Err` case instead of dropping them.)
    fn try_load(
        viewer: &mut Viewer,
        bytes: Vec<u8>,
        focus_hint: usize,
    ) -> Result<(), (String, Vec<u8>)> {
        match xmlparser::parse(&bytes) {
            Ok(flat) => {
                let len = bytes.len();
                viewer.doc = Document {
                    source: Source::Owned(bytes),
                    flat,
                };
                let offset = focus_hint.min(len.saturating_sub(1)) as u32;
                viewer.focus_source_offset(offset);
                Ok(())
            }
            Err(err) => Err((err, bytes)),
        }
    }

    /// Applies a set of simultaneous byte-range replacements as one
    /// undoable edit: mutates the (possibly newly-owned) buffer,
    /// re-parses, and refocuses near `focus_hint`. On a parse error the
    /// buffer is rolled back and the document is left exactly as it was.
    pub fn commit(
        &mut self,
        viewer: &mut Viewer,
        edits: Vec<(Range<usize>, Vec<u8>)>,
        focus_hint: usize,
    ) -> Result<(), String> {
        let mut bytes = Self::take_owned_bytes(&mut viewer.doc);
        let mut spans: Vec<PatchSpan> = edits
            .into_iter()
            .map(|(range, inserted)| PatchSpan {
                range,
                removed: Vec::new(),
                inserted,
            })
            .collect();

        apply_spans(&mut bytes, &mut spans);

        match Self::try_load(viewer, bytes, focus_hint) {
            Ok(()) => {
                self.undo_stack.push(Patch { spans, focus_hint });
                self.redo_stack.clear();
                Ok(())
            }
            Err((err, mut bytes)) => {
                unapply_spans(&mut bytes, &spans);
                viewer.doc.source = Source::Owned(bytes);
                Err(err)
            }
        }
    }

    pub fn undo(&mut self, viewer: &mut Viewer) -> Result<bool, String> {
        let Some(mut patch) = self.undo_stack.pop() else {
            return Ok(false);
        };
        let mut bytes = Self::take_owned_bytes(&mut viewer.doc);
        unapply_spans(&mut bytes, &patch.spans);

        match Self::try_load(viewer, bytes, patch.focus_hint) {
            Ok(()) => {
                self.redo_stack.push(patch);
                Ok(true)
            }
            Err((err, mut bytes)) => {
                // Shouldn't be reachable — `unapply_spans` exactly
                // reverses a patch built from a previously-successful
                // parse — but this is a safety net, not a promise it's
                // unreachable. Restore exactly the state that was loaded
                // before this undo attempt (re-apply the patch) rather
                // than leaving `viewer.doc` pointing at unparsed bytes.
                apply_spans(&mut bytes, &mut patch.spans);
                viewer.doc.source = Source::Owned(bytes);
                self.undo_stack.push(patch);
                Err(err)
            }
        }
    }

    pub fn redo(&mut self, viewer: &mut Viewer) -> Result<bool, String> {
        let Some(mut patch) = self.redo_stack.pop() else {
            return Ok(false);
        };
        let mut bytes = Self::take_owned_bytes(&mut viewer.doc);
        apply_spans(&mut bytes, &mut patch.spans);

        match Self::try_load(viewer, bytes, patch.focus_hint) {
            Ok(()) => {
                self.undo_stack.push(patch);
                Ok(true)
            }
            Err((err, mut bytes)) => {
                unapply_spans(&mut bytes, &patch.spans);
                viewer.doc.source = Source::Owned(bytes);
                self.redo_stack.push(patch);
                Err(err)
            }
        }
    }

    /// Writes the current (possibly edited) document bytes to `path`
    /// as-is — unedited regions are byte-identical to the original file
    /// since we only ever spliced the specific edited spans, never
    /// re-serialized the whole document.
    pub fn save(&self, viewer: &Viewer, path: &Path) -> std::io::Result<()> {
        let tmp_path = path.with_extension("xless.tmp");
        {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(&viewer.doc.source)?;
            f.flush()?;
        }
        std::fs::rename(&tmp_path, path)
    }
}

/// Normalizes a closing-tag row to its opening row — they represent the
/// same logical node, and most edit commands only want to think about
/// one of them. Shared by `node_full_span` below and by app.rs's
/// `rename_focused`/`edit_content`, which used to each inline this same
/// four-line check (found by review as duplicated logic worth a single
/// source of truth, not a correctness bug on its own, but exactly the
/// kind of copy-pasted invariant that's easy to fix in one copy and miss
/// in another).
pub fn resolve_container_row(doc: &Document, index: Index) -> Index {
    if doc.flat[index].is_closing_of_container() {
        doc.flat[index].pair_index().unwrap()
    } else {
        index
    }
}

/// The full source byte span a row's *node* occupies — not just
/// `Row.range` (which for elements is only the tag name). Needed for
/// structural edits (delete, insert-sibling) that must know exactly what
/// text to remove/anchor around. Normalizes a closing-tag row to its
/// opening row's full span (open tag through close tag) first.
pub fn node_full_span(doc: &Document, index: Index) -> Range<usize> {
    let index = resolve_container_row(doc, index);
    let row = &doc.flat[index];
    let start = row.range.start as usize;
    let end = row.range.end as usize;

    match &row.value {
        Value::Text => start..end,
        Value::CData => (start - 9)..(end + 3), // <![CDATA[ ... ]]>
        Value::Comment => (start - 4)..(end + 3), // <!-- ... -->
        Value::ProcessingInstruction => (start - 2)..(end + 2), // <? ... ?>
        Value::DocType => (start - 9)..(end + 1), // <!DOCTYPE ... >
        Value::EmptyElement { .. } => open_tag_span(doc, index),
        Value::OpenElement { close_index, .. } => {
            let open_start = open_tag_span(doc, index).start;
            let close_end = close_tag_span(doc, *close_index).end;
            open_start..close_end
        }
        Value::CloseElement { .. } => unreachable!("normalized above"),
    }
}

/// Span of just `<tag attr="val" ...>` or `<tag attr="val" .../>` (the
/// opening tag only, not its content). Scans forward from the last
/// attribute's value (or the tag name, if none) for the terminating `>` —
/// safe to scan naively there since nothing after the last attribute
/// value can contain a quoted `>` to be fooled by.
fn open_tag_span(doc: &Document, index: Index) -> Range<usize> {
    let row = &doc.flat[index];
    let name_start = row.range.start as usize;
    let scan_from = doc
        .flat
        .attrs_of(index)
        .last()
        .map(|a| a.value.end as usize + 1) // +1 to skip the closing quote
        .unwrap_or(row.range.end as usize);

    let bytes: &[u8] = &doc.source;
    let rel = bytes[scan_from..]
        .iter()
        .position(|&b| b == b'>')
        .expect("well-formed opening tag must contain '>'");

    (name_start - 1)..(scan_from + rel + 1)
}

/// Span of the raw attribute text between a tag's name and its
/// terminating `>` (excluding a self-closing `/`, if present) — what `i`
/// (edit_content, app.rs) lets the user edit directly as one blob rather
/// than per-attribute (ARCHITECTURE.md §7.2 flagged per-attribute rows as
/// a possible fast-follow; this is the simpler v1). Empty (a zero-length
/// range right after the tag name) when the element has no attributes.
pub fn attrs_blob_span(doc: &Document, index: Index) -> Range<usize> {
    let row = &doc.flat[index];
    let name_end = row.range.end as usize;
    let full = open_tag_span(doc, index);
    let gt_pos = full.end - 1; // position of the '>' character itself
    let is_self_closing = matches!(row.value, Value::EmptyElement { .. });
    let blob_end = if is_self_closing && gt_pos > name_end && doc.source[gt_pos - 1] == b'/' {
        gt_pos - 1
    } else {
        gt_pos
    };
    name_end..blob_end
}

/// Span of just `</tag>` (the closing tag).
fn close_tag_span(doc: &Document, close_index: Index) -> Range<usize> {
    let row = &doc.flat[close_index];
    let name_end = row.range.end as usize;
    let bytes: &[u8] = &doc.source;
    let rel = bytes[name_end..]
        .iter()
        .position(|&b| b == b'>')
        .expect("well-formed closing tag must contain '>'");
    ((row.range.start as usize) - 2)..(name_end + rel + 1)
}

/// Escapes `<` and `&` (and, defensively, `]]>`'s closing sequence isn't
/// relevant to plain text) for insertion into an element's text content.
/// Attribute-blob edits deliberately do *not* go through this — see the
/// module doc comment.
pub fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;")
}

/// Best-effort "what row should stay focused" helper: after an edit and
/// reparse, prefer the row occupying the same source position the edit
/// was made at (handled by `EditHistory::reparse` via
/// `Viewer::focus_source_offset`); this just centralizes where callers
/// compute that starting offset for common operations.
pub fn parent_or_self_offset(doc: &Document, index: Index) -> usize {
    match doc.flat[index].parent {
        OptionIndex::Index(p) => doc.flat[p].range.start as usize,
        OptionIndex::Nil => doc.flat[index].range.start as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Source;
    use crate::types::TTYDimensions;
    use crate::viewer::Viewer;

    fn viewer(xml: &str) -> Viewer {
        let flat = xmlparser::parse(xml.as_bytes()).unwrap();
        let doc = Document {
            source: Source::Owned(xml.as_bytes().to_vec()),
            flat,
        };
        Viewer::new(
            doc,
            TTYDimensions {
                width: 80,
                height: 24,
            },
        )
    }

    #[test]
    fn test_node_full_span_element_with_children() {
        let v = viewer("<root><a>1</a></root>");
        // 0 root-open, 1 a-open, 2 text, 3 a-close, 4 root-close
        let span = node_full_span(&v.doc, 1);
        assert_eq!(&v.doc.source[span], b"<a>1</a>".as_slice());
    }

    #[test]
    fn test_node_full_span_empty_element() {
        let v = viewer(r#"<root><a id="1"/></root>"#);
        let span = node_full_span(&v.doc, 1);
        assert_eq!(&v.doc.source[span], br#"<a id="1"/>"#.as_slice());
    }

    #[test]
    fn test_node_full_span_attr_with_gt_in_value() {
        // A literal '>' inside a quoted attribute value must not confuse
        // the open-tag scan.
        let v = viewer(r#"<root><a note="1 > 0">x</a></root>"#);
        let span = node_full_span(&v.doc, 1);
        assert_eq!(&v.doc.source[span], br#"<a note="1 > 0">x</a>"#.as_slice());
    }

    #[test]
    fn test_node_full_span_comment() {
        let v = viewer("<root><!--hi--></root>");
        let span = node_full_span(&v.doc, 1);
        assert_eq!(&v.doc.source[span], b"<!--hi-->".as_slice());
    }

    #[test]
    fn test_commit_delete_and_undo() {
        let mut v = viewer("<root><a>1</a><b>2</b></root>");
        let mut hist = EditHistory::default();
        let span = node_full_span(&v.doc, 1); // <a>1</a>
        let start = span.start;
        hist.commit(&mut v, vec![(span, Vec::new())], start)
            .unwrap();
        assert_eq!(
            crate::lineprinter::pretty_printed(&v.doc),
            "<root>\n  <b>\n    2\n  </b>\n</root>\n"
        );

        hist.undo(&mut v).unwrap();
        assert!(crate::lineprinter::pretty_printed(&v.doc).contains("<a>"));
    }

    #[test]
    fn test_commit_rejects_and_rolls_back_invalid_edit() {
        let mut v = viewer("<root><a>1</a></root>");
        let mut hist = EditHistory::default();
        let before = v.doc.source.len();
        // Replace the opening tag's name range with something that won't
        // match the closing tag, breaking well-formedness.
        let name_range = v.doc.flat[1].range.start as usize..v.doc.flat[1].range.end as usize;
        let result = hist.commit(&mut v, vec![(name_range, b"z".to_vec())], 0);
        assert!(result.is_err());
        assert_eq!(v.doc.source.len(), before);
        assert!(!hist.is_dirty());
    }

    #[test]
    fn test_rename_both_spans_atomically() {
        let mut v = viewer("<root><a>1</a></root>");
        let mut hist = EditHistory::default();
        let open = &v.doc.flat[1];
        let open_range = open.range.start as usize..open.range.end as usize;
        let close_index = open.pair_index().unwrap();
        let close = &v.doc.flat[close_index];
        let close_range = close.range.start as usize..close.range.end as usize;

        hist.commit(
            &mut v,
            vec![(open_range, b"z".to_vec()), (close_range, b"z".to_vec())],
            0,
        )
        .unwrap();

        assert_eq!(
            crate::lineprinter::pretty_printed(&v.doc),
            "<root>\n  <z>\n    1\n  </z>\n</root>\n"
        );
    }

    #[test]
    fn test_undo_multi_span_with_length_change() {
        // Regression test for the unapply_spans ordering bug (found by
        // review): renaming <a> to <verylong> touches two spans (open
        // and close tag names) of *different* length than the original
        // ("a" -> "verylong"), which is exactly the case the old
        // descending-order unapply_spans got wrong — it would splice the
        // wrong byte range for whichever span wasn't at the lowest
        // original offset, corrupting the buffer instead of restoring it.
        let original = "<root><a>1</a></root>";
        let mut v = viewer(original);
        let mut hist = EditHistory::default();

        let open = &v.doc.flat[1];
        let open_range = open.range.start as usize..open.range.end as usize;
        let close_index = open.pair_index().unwrap();
        let close = &v.doc.flat[close_index];
        let close_range = close.range.start as usize..close.range.end as usize;

        hist.commit(
            &mut v,
            vec![
                (open_range, b"verylong".to_vec()),
                (close_range, b"verylong".to_vec()),
            ],
            0,
        )
        .unwrap();
        assert_eq!(
            crate::lineprinter::pretty_printed(&v.doc),
            "<root>\n  <verylong>\n    1\n  </verylong>\n</root>\n"
        );

        // The critical assertion: undo must restore the buffer *exactly*
        // byte-for-byte, not some corrupted splice of the wrong range.
        hist.undo(&mut v).unwrap();
        assert_eq!(
            std::str::from_utf8(&v.doc.source).unwrap(),
            original,
            "undo did not restore the original bytes exactly"
        );

        // And redo must reapply cleanly too.
        hist.redo(&mut v).unwrap();
        assert_eq!(
            crate::lineprinter::pretty_printed(&v.doc),
            "<root>\n  <verylong>\n    1\n  </verylong>\n</root>\n"
        );
    }

    #[test]
    fn test_undo_redo_roundtrip_preserves_exact_bytes_with_shrinking_rename() {
        // Same class of bug, opposite direction: renaming to a *shorter*
        // name (negative length delta) exercises the same span-shift math
        // with the sign flipped.
        let original = "<root><verylong>1</verylong></root>";
        let mut v = viewer(original);
        let mut hist = EditHistory::default();

        let open = &v.doc.flat[1];
        let open_range = open.range.start as usize..open.range.end as usize;
        let close_index = open.pair_index().unwrap();
        let close = &v.doc.flat[close_index];
        let close_range = close.range.start as usize..close.range.end as usize;

        hist.commit(
            &mut v,
            vec![(open_range, b"a".to_vec()), (close_range, b"a".to_vec())],
            0,
        )
        .unwrap();

        hist.undo(&mut v).unwrap();
        assert_eq!(std::str::from_utf8(&v.doc.source).unwrap(), original);
    }
}
