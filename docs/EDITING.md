# Editing model

**Status: implemented in `src/edit.rs`, with one deliberate change from
the design below** — worth reading first since it affects §3 specifically:
rather than the subtree-scoped splice-and-renumber approach §3 describes,
the shipped implementation re-parses the **entire** document on every
committed edit (not every keystroke — see §1). Renumbering every row
index throughout the document that references a position past a subtree
edit (parent/sibling/pair indices are absolute `Vec` positions) is a
correctness-sensitive optimization that deserved its own careful pass
rather than being folded into first getting editing working at all.
Whole-document re-parse is simple, obviously correct, and — since it only
runs once per *confirmed* edit — is a perfectly reasonable interactive
experience even on a 100–200MB file (a brief pause after pressing Enter to
confirm a change, not a stall while typing). The patch/undo model in §2–§4
below is accurate to what's implemented; only the "smallest enclosing
subtree" scoping in §3 is aspirational — `EditHistory` in `edit.rs`
re-parses fully and documents this trade-off in its own module comment.
Also implemented, simpler than §5's original per-keystroke-escaping plan:
every edit is validated by attempting the edit and re-parsing, rolling
back the byte buffer on failure rather than validating character-by-
character as the user types (see `edit.rs`'s module doc comment and
`EditHistory::commit`).

This is the piece jless doesn't have a version of at all — jless is
explicitly read-only. Adding real editing (not just navigation) is a
deliberate scope expansion beyond "a `jless` port for XML," recorded and
reasoned about here in one place. This doc assumes ARCHITECTURE.md §3
(data model), §4 (parsing, `Row.range` into mmap'd original bytes) and §8
(100–200MB performance target) as given — the edit model is designed
*because of* §8, not despite it: whole-buffer text editing (what a normal
text editor does) is fine at small sizes and a bad idea at 200MB, so the
representation below is patch-based from the start.

**Scope, as shipped**: the constrained-structural-operations direction
ARCHITECTURE.md §7.4 flagged as the recommended default is what got
built, with one simplification from the original per-attribute vision:
attributes are edited as one raw text blob (` id="1" active="true"`)
rather than individually. See ARCHITECTURE.md §7.2 for why (attributes
aren't individually addressable rows) — promoting them to per-attribute
editing remains a possible future direction, not a blocker for having a
genuinely useful `i` today.

## 1. Modes and commands (as implemented, `app.rs`)

Normal mode gains, on top of every navigation/collapse command ported
from jless (ARCHITECTURE.md §2/§6):

| Key | Action |
|---|---|
| `i` | Edit the focused row's content: a `Text` row's text, a `Comment`/`CData`/`ProcessingInstruction`/`DocType` row's raw content, or — focused on an element — its whole raw attribute blob |
| `r` | Rename the focused element's tag (renames both its opening and closing tag atomically, one undo step) |
| `o` / `O` | Prompt for a new element name, insert `<name/>` as the next/previous sibling of the focused node |
| `dd` | Delete the focused node's full subtree |
| `yy` / `yl` / `yt` / `yn` / `yx` | Yank to the system clipboard: pretty-printed subtree / one-line subtree / concatenated text content / tag name / XPath (`path.rs`) |
| `p` / `P` | Paste the system clipboard's current text as the next/previous sibling of the focused node |
| `u` / `Ctrl-r` | Undo / redo |
| `:w`, `:w file.xml`, `:wq`, `:q!` | Save (in place / to a new file), save-and-quit, quit discarding pending edits |

"Insert mode" is a single-line text-entry overlay on the status bar
(`App::read_line` in `app.rs`) prefilled with the current value where
there is one (rename, edit-content) — not a readline library (jless uses
`rustyline`, which needs the `/dev/tty` remap dance to coexist with piped
stdin; this is simpler and doesn't need that — see `input.rs`'s module
doc comment) and not full-screen: you're always editing exactly one field,
confirmed with `Enter` or cancelled with `Esc`/`Ctrl-C`. Horizontal
scrolling for a field wider than the terminal isn't implemented (same
scope cut as `lineprinter.rs`'s missing `truncatedstrview.rs` port — see
that module's doc comment); a very long value will just run off-screen
while typing.

Yank/paste round-trips through the *real* system clipboard
(`clipboard.rs`, shelling out to `pbcopy`/`pbpaste`, `wl-copy`/`wl-paste`,
or `xclip`/`xsel` — deliberately not the native `clipboard` crate jless
uses, which pulls in X11 dev headers on Linux; see `clipboard.rs`'s
module doc comment), so it's not an internal-only register the way this
doc originally sketched it — copy in xless, paste in your editor, or vice
versa, all work.

## 2. Why not just edit the in-memory row/text buffers directly

`Row.range` values are byte ranges into the mmap'd original file (or, for
small files under some threshold, a `String` — see §3). Splicing text into
the *middle* of that buffer would invalidate every subsequent row's
range in the whole file — an insertion at byte 1000 shifts every range
after it. Re-deriving all of them on every keystroke is exactly the
whole-file-rescan cost §8 exists to avoid. So edits never mutate the
source buffer or the `Row` ranges in place; they're tracked as an overlay
instead.

## 3. Patch overlay

An edit is a `(Range<usize> in original bytes, replacement: String)` —
"replace this span of the *original* file with this new text." All
pending edits live in a sorted `Vec<Patch>` (sorted and non-overlapping by
construction, since each command targets one row's own range, and ranges
are re-validated against already-applied patches before a new one is
added).

- **Applying a structural command** (rename tag, set attribute, replace
  text, insert element) computes the new patch from the focused row's
  existing `range`/attribute sub-range, without touching anything else.
- **Rebuilding the affected `Row`s**: after a patch is added, only the
  smallest enclosing element (walk `parent` links from the edited row —
  already available from the ported `flatjson.rs` traversal, §2) gets
  re-parsed: run `xmlparser.rs` on that element's original byte range
  with the new patch spliced in, producing a small `FlatXml` fragment,
  then splice that fragment's rows into the full row vector in place of
  the old ones (renumbering indices for the spliced range — an
  `O(subtree size)` operation, not `O(file size)`). This is the direct
  payoff of §8.4's phase-1/phase-2 split and §8.3's compact indices: the
  machinery for "reparse just this subtree" already has to exist for lazy
  detail parsing, editing reuses it.
- **Undo/redo**: the patch list doubles as the undo log. Undo = remove
  the last patch and re-run the same splice-rebuild step against the
  original (unpatched) byte range; redo = re-add it. No separate
  undo-specific data structure needed.

## 4. Saving

`:w` streams output by walking the original mmap'd bytes and the sorted
patch list together: copy unchanged spans straight through (no
re-serialization, no full-file buffer), substitute patched spans with
their replacement text. This is the same technique line-based patch tools
use, applied to byte ranges instead of lines, and it means saving a
200MB file with a five-character edit costs roughly "stream 200MB through
a writer," not "hold a second 200MB copy in memory to mutate." `:w
newfile.xml` does the same walk, writing to a different path instead of
overwriting in place (in-place overwrite needs the usual
write-to-temp-then-rename dance to avoid corrupting the file on a crash
mid-write).

## 5. Keeping edits well-formed — as implemented

The shipped safety net (`edit::EditHistory::commit`) is simpler than the
per-keystroke validation originally sketched here, and uniform across
every edit kind instead of needing separate rules per field type: apply
the edit to a scratch copy of the byte buffer, try to re-parse the whole
document, and — if that fails — restore the buffer to exactly what it was
and surface the parse error (`edit rejected: <message>` in the status
bar) rather than leaving a broken document loaded. Verified in
`edit.rs`'s `test_commit_rejects_and_rolls_back_invalid_edit` and by hand
against a real running session (typing an unmatched `<not-closed` into a
field and confirming the file on disk was untouched after `:w`).

This trades "reject bad keystrokes as you type" for "validate on
confirm, roll back cleanly if invalid" — simpler to implement correctly
than character-by-character escaping/validation, and arguably more
predictable (a clear named error, not silent auto-correction). One piece
of the original per-field-escaping idea *did* ship: text-content edits
(not raw attribute-blob edits — see §1) auto-escape `<` and `&` on the
way in (`edit::escape_text`), since typing a literal `&` or `<` into
"this element's text" is an extremely natural thing to want to do without
thinking about XML escaping rules.

## 6. Interaction with vim launch — resolved

`VIM_INTEGRATION.md` §3's temp-file trick (open a scratch copy when the
vim buffer has unsaved changes, so xless shows your actual edits) was
designed when xless was view-only, before editing existed. The shipped
`contrib/vim/plugin/xless.vim` handles the guard this section originally
flagged as unresolved: when it writes a scratch copy, it prints an
explicit `echom` warning naming the temp path and stating that edits made
inside that xless session won't flow back into the vim buffer. It doesn't
force the session read-only — xless's own status bar already shows
whichever path it's actually operating on (the temp file's, in that case,
not your real filename), which combined with the one-time warning was
judged sufficient rather than removing the ability to edit a
just-unsaved-buffer's content entirely. Revisit if this turns out to be
confusing in practice.
