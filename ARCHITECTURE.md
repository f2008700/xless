# xless — Architecture Note (as built)

This note describes how xless is **actually implemented today** (the code
in `src/` as of commit `1fc2061` plus the bug-fix and large-file pass
recorded in [§15](#15-fixed-issues-and-large-file-verification)). It is the companion to `README.md`'s
*Architecture* / *Editing model* sections, which are primarily a design
record — how the jless port was planned, what was considered, and why.
Where the shipped code diverges from that design, this note says so
explicitly (see [§14 Design vs. implementation](#14-design-vs-implementation-drift)).

---

## 1. One-paragraph summary

xless parses an XML file once (via `quick-xml`, zero-copy) into a
**flat vector of compact rows** (36 bytes each) — one row per
element-open, element-close, text, comment, CDATA, PI, or doctype — where
every row carries a parent index, its open/close pair index and a **byte
range into the original source bytes** (mmap'd for files); sibling and
child links are derived from row order rather than stored. Everything
else operates on that vector: navigation is index arithmetic over it,
rendering formats only the visible rows each frame, search scans it,
paths walk its parent links (with a per-parent sibling-position cache). Editing splices bytes into an owned copy of the source and
**re-parses the whole document** on every committed edit, rolling back if
the result isn't well-formed. When stdout isn't a TTY the program just
pretty-prints and exits; otherwise it runs a termion-based full-screen
event loop.

---

## 2. Two execution paths

`main.rs` decides between them up front:

```
Opt::parse()                          options.rs (clap)
  │
load_source()                         file → memmap2::Mmap   (Source::Mapped)
  │                                   "-" / no arg + piped stdin → Vec<u8> (Source::Owned)
xmlparser::parse(&source) ──Err──▶ eprintln + exit(1)
  │
Document { source, flat }
  │
stdout is a tty? ──no──▶ lineprinter::write_pretty_printed(&doc, stdout); return  ← PIPE MODE
  │ yes                                                (streamed in 64KB chunks; EPIPE = normal exit)
config::load_or_init(~/.xless/settings.json) ──Err──▶ eprintln + exit(1)     ← before raw mode
  │
Viewer::new(doc, terminal size)       scrolloff, mode, reserved_rows = 1 + HEADER_ROWS
  │   └─ --focus-line → byte offset → viewer.focus_source_offset()
  │
stdin not a tty (document was piped in)? → dup2(/dev/tty → fd 0)       ← keys come from the terminal
  │
raw mode → alternate screen → HideCursor → MouseTerminal   (termion wrappers, RAII restore)
  │
App::new(viewer, filename, path, keymap, stdout).run(input::get_input())   ← INTERACTIVE
```

Design points worth knowing:

- **Pipe mode never reads the keymap**, so a broken `settings.json` can't
  break `xless f.xml | grep …` or vim's `:r !xless %`.
- **All fatal validation (input XML, config) happens before the terminal
  is put into raw/alternate-screen mode**, so errors print as normal
  stderr text rather than inside a half-drawn TUI.
- Terminal restoration is purely RAII: dropping the termion wrapper chain
  when `main` returns restores cooked mode / the main screen.

---

## 3. Module map

| Layer | Module | LoC | Responsibility | jless lineage |
|---|---|---:|---|---|
| Entry | `main.rs` | 247 | Load input, choose pipe vs. interactive, set up terminal | adapted |
| | `options.rs` | 80 | clap `Opt`: `--mode`, `-n/-N`, `-r/-R`, `--scrolloff`, `--focus-line` | adapted |
| Data model | `document.rs` | 57 | `Source` (mmap or owned bytes) + `FlatXml` → safe `&str` slices | new |
| | `flatxml.rs` | 701 | `Row`/`Value`/`Attr`/`FlatXml`, derived sibling/child links, traversal, collapse, offset→row lookup | port of `flatjson.rs` traversal |
| | `xmlparser.rs` | 584 | quick-xml pull parser → `FlatXml`, `xml:space` scopes, extra well-formedness checks | new |
| View state | `viewer.rs` | 664 | Cursor, scroll window, Line/Compact mode, `Action` state machine | close port |
| Rendering | `screenwriter.rs` | 312 | Frame composition: header, gutter, rows, status bar, prompt line, help pager | adapted |
| | `lineprinter.rs` | 491 | One row → one line of text; streaming whole-doc / subtree pretty-print; text content | rewritten |
| | `highlighting.rs` | 55 | Row kind → color; header/search/gutter colors | rewritten |
| | `terminal.rs` | 217 | ANSI escape abstraction, buffered into a `String`, style de-dup | near-verbatim |
| | `types.rs` | 17 | `TTYDimensions` | verbatim |
| Control | `app.rs` | 887 | Event loop, key dispatch, prompts, editing commands, `:` commands, help | adapted + new |
| | `input.rs` | 220 | stdin bytes + SIGWINCH multiplexed with `poll(2)` → `TuiEvent` | adapted |
| | `config.rs` | 944 | Action table, default keymap, `settings.json` load/validate/generate | new |
| Features | `search.rs` | 235 | Regex search over per-row text, wrap-around | adapted in spirit |
| | `path.rs` | 382 | XPath (`/a[1]/b[2]`) and literal breadcrumb (`a/b[2]`) builders, `PathCache` | new |
| | `edit.rs` | 886 | Byte-span patches, commit/undo/redo with reparse+rollback, collapse carry-over, save, span helpers | new |
| | `clipboard.rs` | 82 | Shell out to `pbcopy`/`wl-copy`/`xclip`/`xsel` (and paste counterparts) | new |
| Integration | `contrib/vim/` | ~80 | `:Xless` command + `<leader>xx` ftplugin mapping | new |

### Dependency direction

```
                        main
                 ┌───────┼──────────────┐
                 ▼       ▼              ▼
              options   app ─────────▶ input
                         │  \
        ┌────────┬───────┼───┬──────┬──────────┐
        ▼        ▼       ▼   ▼      ▼          ▼
     config   search   edit path clipboard screenwriter
                 │       │   │                 │  │  └─▶ terminal, highlighting
                 │       ├──▶ viewer ◀─────────┘  │
                 │       │      │                 ▼
                 │       │      │            lineprinter
                 ▼       ▼      ▼                 │
                 └──────▶ document ◀──────────────┘
                            │
                            ▼
                         flatxml ◀── xmlparser (quick-xml)
```

Only `flatxml`, `xmlparser`, `lineprinter`, `highlighting`, `path`,
`search` and `edit` know anything about XML. `terminal`, `input`, `types`
and most of `viewer` are format-agnostic (the property that made the jless
port cheap).

---

## 4. Core data model

### 4.1 `Document` and `Source` (`document.rs`)

```rust
pub enum Source { Mapped(memmap2::Mmap), Owned(Vec<u8>) }   // Deref<Target=[u8]>
pub struct Document { pub source: Source, pub flat: FlatXml }
```

`Document` is the single owner of both the bytes and the rows that point
into them, which is what lets `Document::text(range)` hand out `&str`
slices safely. The whole source is UTF-8-validated once in
`xmlparser::parse`, so `text()` uses checked `from_utf8` only as a
belt-and-braces assertion (a failure would mean a parser bug).

`Source::Mapped` is used for real files; stdin and every post-edit buffer
are `Source::Owned`.

### 4.2 `FlatXml`, `Row`, `Value`, `Attr` (`flatxml.rs`)

```rust
pub type Index = u32;                     // NIL = u32::MAX
pub enum OptionIndex { Nil, Index(Index) } // the API type; not stored in rows

pub struct Row {                          // 36 bytes (guarded by test_row_is_compact)
    parent: Index,                        // NIL at top level; read via Row::parent() -> OptionIndex
    pub depth: u32,
    pub index_in_parent: u32,             // among ALL siblings (not XPath-style)
    pub range: Range<u32>,                // bytes in the ORIGINAL source
    pub value: Value,                     // 16 bytes
}

pub enum Value {
    Text, CData, Comment, ProcessingInstruction, DocType,   // "primitives"
    EmptyElement { attrs: Range<u32> },                       // <a/>
    OpenElement  { collapsed, attrs: Range<u32>, close_index: Index },
    CloseElement { collapsed, open_index: Index },
}

pub struct Attr { name: Range<u32>, value: Range<u32> }      // raw, still-escaped value
pub struct FlatXml { rows: Vec<Row>, attrs: Vec<Attr> }      // attrs = side table
```

**Derived links.** Sibling and child links are *not* stored. Because rows
are in source order and every subtree is the contiguous slice
`open..=close`, each link is an O(1) look at a neighbouring row:

| `FlatXml::…(i)` | Rule |
|---|---|
| `first_child` | open row whose `close_index > i + 1` → `i + 1`, else `Nil` |
| `last_child` | close row `c` with children → the node owning row `c − 1` (its opening row if `c − 1` is a closing tag) |
| `prev_sibling` | `Nil` if `i == 0`, if `i` is a close row, or if row `i − 1` is an opening tag (`i` is a first child); else the node owning row `i − 1` |
| `next_sibling` | `Nil` for close rows; else the row after this node's last row, unless that's a closing tag (the parent's) or EOF |

An earlier layout stored all four as 8-byte `OptionIndex` enums (64-byte
rows); at 150MB of typical XML (~14M rows) that was ~0.9GB of index.
`test_derived_links_match_brute_force` checks the rules against a
brute-force reference on mixed content, `<a></a>`, deep nesting and
prolog/epilog siblings.

What `Row.range` covers depends on the kind — this matters for every
module that reconstructs markup:

| Kind | `range` covers | Delimiters re-added by renderers / `edit::node_full_span` |
|---|---|---|
| `OpenElement` / `CloseElement` / `EmptyElement` | tag **name only** | `<`, attrs, `>` / `</`, `>` / `/>` |
| `Text` | the text, raw (entities not unescaped) | none |
| `CData` | content | `<![CDATA[` … `]]>` (9 / 3 bytes) |
| `Comment` | content | `<!--` … `-->` (4 / 3) |
| `ProcessingInstruction` (incl. `<?xml …?>`) | content | `<?` … `?>` (2 / 2) |
| `DocType` | content after `<!DOCTYPE` | `<!DOCTYPE` … `>` (9 / 1) |

### 4.3 Worked example

`<root><a>1</a><b x="y"/></root>` becomes:

| idx | value | range→ | parent (stored) | stored pair / attrs | derived: prev/next sib, first/last child |
|---:|---|---|---|---|---|
| 0 | `OpenElement` | `root` | Nil | close_index=5 | Nil / Nil, first_child=1 |
| 1 | `OpenElement` | `a` | 0 | close_index=3 | Nil / **4**, first_child=2 |
| 2 | `Text` | `1` | 1 | | Nil / Nil |
| 3 | `CloseElement` | `a` | 0 | open_index=1 | last_child=2 |
| 4 | `EmptyElement` | `b` | 0 | attrs=0..1 → (`x`, `y`) | **1** / Nil |
| 5 | `CloseElement` | `root` | Nil | open_index=0 | last_child=4 |

### 4.4 Invariants everything relies on

1. **Rows are in source order** and `range.start` is monotonically
   increasing → `row_at_source_offset` can binary-search.
2. **A container's subtree is the contiguous slice `open..=close`.**
   Collapsed-skip, subtree yank, text content, delete — and now every
   sibling/child link (§4.2) — depend on this.
3. **Close rows are not sibling-linked.** They copy `parent`, `depth` and
   `index_in_parent` from their open row, but `prev_sibling`/`next_sibling`
   return `Nil` for them, so sibling iteration visits each node once.
4. **`collapsed` is stored on both halves** of an open/close pair and is
   only ever changed through `FlatXml::{expand,collapse,toggle_collapsed}`,
   which keep them in sync.
5. **Attributes are not rows.** They live in the `attrs` side table,
   referenced by `Range<u32>` from the owning element, and are always
   rendered inline in the opening tag.
6. **One row = one terminal line** in the interactive view: `render_row`
   replaces `\n`, `\r`, `\t` with spaces (`sanitize_for_line`). The
   pipe-mode/yank printers never do this (§7.2).
7. **`<a></a>` is an Open/Close pair with `close_index == open + 1`** (no
   children), not an `EmptyElement`; only literal `<a/>` produces
   `EmptyElement`. Compact mode *renders* the former as `<a/>`.
8. **Whitespace-only text nodes are dropped** at parse time, except inside
   an `xml:space="preserve"` scope (inherited by descendants; turned off
   again by `xml:space="default"`). There's no `--show-whitespace` flag.

### 4.5 Traversal primitives

Two notions of "next", ported from jless:

- `next_visible_row` / `prev_visible_row` — the literal next row,
  jumping over the body of a collapsed container (open→its close, and
  backward from a collapsed close to its open).
- `next_item` / `prev_item` — same, but additionally skips **all**
  `CloseElement` rows.

Plus `last_visible_index` / `last_visible_item`, `row_at_source_offset`
(binary search that prefers the *next* row when the offset falls in the
inter-token gap, so a line's leading indentation maps to the row that
starts that line), and `first_visible_ancestor` (ported, currently
unused).

---

## 5. Parsing (`xmlparser.rs`)

Single pass over `Reader::from_str(text)` with `trim_text(false)`:

| quick-xml event | Action |
|---|---|
| `Start` | push `OpenElement`, take the next `index_in_parent` slot, push onto `open_stack` with its `xml:space` scope |
| `Empty` | push `EmptyElement`, take the next slot |
| `End` | pop stack + scope, push `CloseElement` (copies open row's parent/depth/index), back-patch `close_index` |
| `Text` | drop if all-ASCII-whitespace and not in a preserve scope; else validate, push `Text` |
| `CData` / `Comment` / `PI` / `Decl` / `DocType` | push the corresponding leaf (`Decl` → `ProcessingInstruction`) |

**Zero-copy ranges.** Every event's bytes borrow from the input, so
`offset()` computes `bytes.as_ptr() - base` to get the range. A
`Cow::Owned` attribute value would mean quick-xml re-encoded something;
that's treated as a bug and panics (the `encoding` feature is
deliberately off).

**Per-level state** is two stacks parallel to `open_stack`:
`level_child_count` (for `index_in_parent`) and `space_preserve` (the
`xml:space` scope). No sibling links are built — `FlatXml` derives them.

**Well-formedness checks added on top of quick-xml**, because quick-xml's
raw reader accepts these and the edit safety net (§10) relies on the
parser being strict:

- exactly one top-level element (`top_level_elements == 1`), no unclosed
  elements, non-empty document;
- every `&` in text/attribute values starts a syntactically valid entity
  or character reference (`validate_references`);
- no literal `]]>` in ordinary text (`validate_no_cdata_terminator`).

Any error aborts the whole parse with a message — there is no partial or
lenient mode.

---

## 6. View state and navigation (`viewer.rs`)

`Viewer` owns the `Document` plus view state:

```rust
pub struct Viewer {
    pub doc: Document,
    pub top_row: Index,          // first row drawn
    pub focused_row: Index,      // cursor
    desired_depth: u32,          // sticky depth for J/K sibling moves
    pub dimensions: TTYDimensions,
    pub reserved_rows: u16,      // chrome rows (status bar + header) — set by main
    pub scrolloff_setting: u16,
    pub mode: Mode,              // Line | Compact
}
```

Collapse state lives inside `doc.flat` (on the rows), not in `Viewer`.

**Modes change traversal, not just rendering.** `next_display_row` /
`prev_row` / `last_line` dispatch to `next_visible_row`-family in
`Mode::Line` and to `next_item`-family in `Mode::Compact`. So in Compact
mode closing tags are never on screen and never focusable; in Line mode
they are real rows. `screenwriter` uses the same `next_display_row` to
step through the frame, so what's drawn and what's navigable can't
disagree.

**`perform_action(Action)`** is the single entry point. After each action
it optionally (a) resets `desired_depth` to the focused row's depth and
(b) calls `ensure_focused_row_is_visible()`. Both are decided by
exclusion lists (`should_reset_desired_depth`, `should_refocus_window`):
scroll/page/zt/zz/zb-style actions move the window, not the cursor, so
they skip (b).

**Scrolling** follows jless's algorithm: keep `scrolloff` rows of padding,
recenter if the cursor jumped more than ~1⅓ screens, and never scroll past
EOF padding. `content_height()` = `dimensions.height - reserved_rows` is
the one number all of that math should read; `reserved_rows` exists so
adding the header row didn't silently skew scrolling by one.

**Making a row visible** (`focus_row_expanding_ancestors`) expands every
collapsed ancestor rather than redirecting to the nearest visible one.
Search, `--focus-line`, `NG` and post-edit refocus all go through it.

**Compact mode never focuses a closing tag.** `displayable_row` maps a
close row to its opening row for `JumpTo` (e.g. `42G`, which counts *all*
rows) and after toggling Line→Compact; `ensure_top_row_is_visible` also
moves the window off a close row in Compact mode.

---

## 7. Rendering

### 7.1 Frame composition (`screenwriter.rs`)

Every redraw is a **full repaint** into an in-memory buffer
(`AnsiTerminal.output: String`), flushed with one `write` + `flush`:

```
row 1             ┌ header: string built by App via path::PathCache (XPath or literal), bold on blue
rows 2..H-1       │ for each visible row from top_row (via viewer.next_display_row):
                  │   [gutter: absolute or relative line number] + "  "×depth + render_row()
                  │   style = highlighting::style_for(kind); inverted if focused;
                  │   search-match background if == search.current_match
                  │   truncate_to_width(content_width)   ← unicode-width aware, hard cut
row H             └ status bar: message, or "file [+]  [MODE, row i/N]  /term  :help" (inverted)
```

- Gutter width is sized from `doc.flat.len()` (the largest number that can
  appear), not from screen height.
- "Line numbers" are **row indices + 1**, not source-file line numbers.
- There's no horizontal scrolling: jless's `truncatedstrview.rs` wasn't
  ported, so long lines get cut off at the right edge.
- `print_input_line` repaints only the status row (used by prompts once
  per keystroke). `print_help` draws the help overlay pager.

### 7.2 Row text (`lineprinter.rs`)

Two deliberately separate kinds of output:

- **`render_row(doc, i, mode)`** — the interactive one-liner. A collapsed
  element renders as `<tag attrs>…</tag>` in O(1), whatever the subtree
  size. An expanded, childless open element renders as `<tag attrs/>` in
  Compact mode and `<tag attrs>` in Line mode. Embedded newlines and tabs
  are flattened to spaces.
- **The full-fidelity printers**, which never change content:
  - `write_pretty_printed(doc, w)` — pipe mode. It streams to `w` in 64 KB
    chunks, and a closed pipe (`| head`) counts as a normal exit.
  - `pretty_printed_subtree` (`yy`), and `pretty_printed`, which is
    test-only.
  - `one_line_subtree` (`yl`), which keeps text byte-for-byte.

  Specifically:
  - Only inter-node layout whitespace is re-flowed: text is trimmed at its
    ends, and its interior, including newlines and tabs, is kept.
  - CDATA, comments and PIs are emitted verbatim.
  - Each attribute keeps its **original quote character** (`attr_quote`
    reads the byte before the value). Re-quoting `a='say "hi"'` with `"`
    would be malformed.
  - Anything inside an `xml:space="preserve"` scope is copied verbatim
    from the source on one line.
- **`text_content`** — `.textContent`-style concatenation of `Text` and
  `CData` descendants (`yt`).

### 7.3 Colors and escapes (`highlighting.rs`, `terminal.rs`)

Each row gets **one color for the whole line**, chosen by its kind (tags light
blue, comments grey, CDATA magenta, PI/doctype cyan, text white). The
`ATTR_NAME`/`ATTR_VALUE`/`PUNCTUATION` constants are reserved for future
per-token highlighting. `AnsiTerminal` keeps track of the current style and only
emits SGR codes when something changes. It uses 256-color escapes
(`38;5;n`) with 16-color indices.

---

## 8. Input and the event loop

### 8.1 `input.rs`

`get_input()` registers a SIGWINCH self-pipe (`signal_hook::low_level::pipe`)
and returns `TuiInput`, an `Iterator<Item = io::Result<TuiEvent>>`
that `poll(2)`s **{sigwinch pipe, stdin}** with an infinite timeout.
Bytes are read into a 1 KiB buffer and decoded by termion's `parse_event`.
A lone `ESC` byte in a read is reported as `Key::Esc` (no escape-timeout
logic). Events: `KeyEvent`, `MouseEvent`, `WinChEvent`, `Unknown(bytes)`.
It always reads fd 0; when the document itself was piped in, `main.rs`
has already pointed fd 0 at `/dev/tty`.

### 8.2 `App::run` (`app.rs`)

```
draw()
for event in input:
    message = None                         # status messages live exactly one frame
    if showing_help: help pager owns input (configured move/page/top/bottom/quit keys + arrows/Esc); draw; continue
    WinCh  → re-query size → viewer + screenwriter dimensions
    Mouse  → Left click on a content row → Action::Click(row − HEADER_ROWS − 1), wheel → Scroll(3)
    Key    → handle_key(key, &mut input)
    Quit? break : draw()
```

`handle_key` precedence, in order:

1. **Digits** go into `count_buffer` (except a leading `0`, which falls through).
2. **A pending prefix** (`DeletePrefix` / `YankPrefix`) consumes this key:
   - the same delete key again → delete;
   - a key in `keymap.yank_targets` → yank that target;
   - anything else cancels.
3. **`Ctrl-c`** quits immediately, even with unsaved edits. **`Esc`** clears the
   count. Neither can be remapped.
4. **`keymap.actions.get(&key)`** gives an `AppAction`. If nothing is bound,
   the key is ignored.
5. A `match` on the `AppAction` either becomes a `viewer::Action` (taking the
   count where that makes sense) or calls an `App` feature method (search, yank,
   edit, command mode).

**Prompts are nested modal loops.** `read_line(input, prompt, initial)`
pulls events from the *same* input iterator until Enter or Esc/Ctrl-c, and
repaints only the status line as you type. Search (`/`, `?`), command mode
(`:`), rename (`r`), edit content (`i`) and insert sibling (`o`/`O`) all use
it. That's why `handle_key` takes `&mut input`. There's no readline library,
no history and no cursor movement within the line.

**Command mode** recognises `q`, `quit`, `q!`, `quit!`, `w`, `w <path>`, `wq`,
`set [no]number`, `set [no]relativenumber`, `h` and `help`.

### 8.3 Keybinding configuration (`config.rs`)

- `ACTIONS: &[ActionInfo]` and `YANK_TARGETS: &[YankTargetInfo]` are the
  **single source of truth** for the `AppAction` ↔ JSON name mapping, the
  default keys, and the `:help` text. The help text is rebuilt every frame from
  the live keymap, so it always matches the current bindings.
- `Keymap` is two `HashMap<Key, …>`s (actions, yank targets).
- `load_or_init(path)`: if the file is missing, it writes a complete default
  file and uses the defaults. Otherwise it parses strictly:
  - unknown action names are rejected;
  - unparseable key specs are rejected;
  - a key claimed by two actions is rejected;
  - reserved keys are rejected: `Esc`, `Ctrl-c`, and digits 0–9 (digits are
    taken by the count prefix before the keymap is ever consulted).
- An override's key list **replaces** that action's defaults. To keep swaps
  like j↔k from looking like conflicts, `build_keymap` works in two phases:
  first it clears the defaults of every overridden action, then it inserts the
  new keys.
- A **side effect worth knowing**: the first interactive run creates
  `~/.xless/settings.json`.

---

## 9. Feature modules

### 9.1 Search (`search.rs`)

`SearchState` stores the term, the compiled `Regex`, the direction, and
`current_match`. `find(doc, from, dir)` scans **linearly from `from`,
wrapping around**. For each row it builds a fresh `String`
(`searchable_text`):

- for elements: the tag name plus ` name="value"` for each attribute;
- for leaves: the raw content;
- for close rows: an empty string (they're never targets).

A hit is focused with `Action::JumpTo`, which expands its ancestors, and then
centred on screen. Only the *current* match is highlighted. `find` prefers
any *other* match, but returns `from` itself when it's the only one, and
the status bar then says "this is the only match". The cost per jump is
O(distance to the next match), with one allocation per row visited. At
150 MB that measured about 50 ms per jump.

### 9.2 Paths (`path.rs`)

- `build_xpath` — `/root[1]/item[3]/text()`.
  - The `[n]` predicate is always included. It's 1-based and counts only
    siblings with the same tag.
  - Leaves map to `text()`, `comment()` or `processing-instruction()`.
  - `DocType` is an `Err`, because DOCTYPE has no XPath node.
- `build_literal_path` — `root/item[3]/text`. It only adds `[n]` when
  another sibling has the same tag.
- **`PathCache`** memoises one `SiblingTable` per parent: the same-tag
  position of each child, indexed by `index_in_parent`, plus a total count
  for each tag. A table is built the first time anything asks for that
  parent, in one pass over its children.
  - `App` owns the cache and uses it for the header every frame and for
    `yx`.
  - Building a path is O(depth) after the first use of each parent. Before
    the cache, a node near the end of a long sibling list re-walked every
    earlier sibling on each redraw, about 45–90 ms at 150 MB.
  - **`App` clears the cache on every successful commit, undo or redo**,
    because row indices are only stable between edits.
- The free functions `build_xpath`, `build_literal_path` and `build_path`
  build a throwaway cache, for one-off callers and tests.
- Neither builder produces `@attr` segments.

### 9.3 Clipboard (`clipboard.rs`)

Tries `pbcopy`, `wl-copy`, `xclip` and `xsel` in that order (for paste:
`pbpaste`, `wl-paste`, `xclip -o`, `xsel -o`), always reaping the child
process. If none of them work, yank shows a 40-char preview in the status bar
instead of failing silently. This avoids the native `clipboard` crate and its
X11 build dependency.

---

## 10. Editing (`edit.rs` + `app.rs`)

### 10.1 Edit commands → byte edits

Each command reduces to a `Vec<(Range<usize>, Vec<u8>)>` of byte replacements
plus a `focus_hint` offset:

| Command | Spans | Notes |
|---|---|---|
| `r` rename | open-tag name range (+ the close-tag name range for containers) | one undo step |
| `i` on Text | the text range → `escape_text(input)` | auto-escapes `&` and `<` |
| `i` on Comment/CDATA/PI/DocType | content range → raw input | |
| `i` on an element | `attrs_blob_span` (from the end of the tag name to `>` or `/>`) → raw input | *not* escaped; the reparse catches a bare `&` |
| `o` / `O` | zero-width insert of `<name/>` at the end/start of `node_full_span` | |
| `p` / `P` | zero-width insert of the trimmed clipboard text | can be any markup; the reparse validates it |
| `dd` | `node_full_span` → empty | refocuses on the parent's offset |

`node_full_span`, `open_tag_span` and `close_tag_span` rebuild a node's full
source extent from `Row.range` and the known delimiter lengths. For open tags
they scan forward for `>` starting **after the last attribute value**, so a
`>` inside a quoted value can't confuse them.

### 10.2 Commit / undo / redo

```
commit(viewer, edits, focus_hint):
    collapsed = collapsed_offsets(doc)   # tag-name offsets of collapsed elements
    bytes = take_owned_bytes(doc)        # Mapped → full Vec copy (first edit); Owned → moved out
    apply_spans(bytes, spans)            # records `removed`, splices right-to-left (descending start)
    doc.flat = empty                     # free the old row index BEFORE building the new one
    match xmlparser::parse(bytes):
        Ok(flat)  → doc = Document{Owned(bytes), flat}
                    restore_collapsed(collapsed, remapped through the spans)
                    focus_source_offset(focus_hint)
                    undo_stack.push(Patch{id: next_id++, ..}); redo_stack.clear()
        Err(e)    → unapply_spans(bytes) (ascending start)
                    restore_after_failed_load: reparse the restored bytes (same rows, same indices)
                    and re-apply the collapse state; return Err(e)
```

- **The whole document is re-parsed on every confirmed edit.** Typing into
  a prompt doesn't trigger it; only pressing Enter does.
- **Peak memory is one row index plus one buffer.** The old index is
  dropped before the new parse. The price is a second parse on a *rejected*
  edit: about 2.2 s at 150 MB, compared with about 1.1 s for an accepted
  one.
- **Collapse state survives edits.** Each collapsed element's tag-name
  offset is remapped through the edit's spans (`remap_offset`):
  - offsets before an edit are unchanged;
  - offsets after it shift by the edit's length change;
  - an edit that *starts* at the offset (renaming that tag) keeps it in
    place;
  - an offset strictly inside replaced text is dropped (that element was
    deleted or rewritten).

  Undo uses the inverse mapping (`unapply_mapping`).
- `unapply_spans` has to go in **ascending** start order. That's what makes
  each span's shift cancel exactly. Descending order was a real bug,
  covered by `test_undo_multi_span_with_length_change`.
- After a successful commit, undo or redo, `App` clears both
  `search.current_match` and the `PathCache`, because row indices have all
  changed.
- **Dirty tracking compares patch identities, not stack lengths.**
  - `saved_id` is the id of the top patch at the last save (`None` means
    no patches).
  - `is_dirty()` is `undo_stack.last().id != saved_id`.
  - Ids are never reused, so edit → save → undo → a *different* edit is
    correctly reported as dirty.

### 10.3 Save

`save(path)` writes the whole current buffer to a sibling temp file
(`<name>.xless.tmp`), calls `fsync`, and `rename`s it over the target.

- The replace is atomic, and regions you didn't edit come out
  byte-identical, because nothing is ever re-serialised.
- Rename replaces a directory entry, so two cases are handled:
  - **Symlinks are resolved first** (`canonicalize`), and the real file is
    the one replaced. The link itself stays a link.
  - **The existing file's permission bits** are copied onto the temp file
    before the rename.
- Ownership isn't preserved, since that needs privileges.
- On failure, the temp file is removed.
- `:w <path>` also switches `file_path` to the new target.

---

## 11. Vim integration (`contrib/vim/`)

The plugin needs no special support in xless beyond `--focus-line`:

- **`:Xless [file]`** opens xless in a vertical `:terminal` split, using
  `termopen` on neovim, `vertical terminal` on vim 8+, and `!` as a fallback.
  It passes `--focus-line=<cursor line>`.
- **If the buffer has unsaved changes**, the plugin writes them to a temp file
  first and warns that edits made in xless won't flow back into the buffer.
- **`ftplugin/xml.vim`** maps `<leader>xx`.

On the xless side, `--focus-line` works like this:

1. `main::byte_offset_of_line` turns the line number into a byte offset.
2. `row_at_source_offset` finds the matching row.
3. The viewer expands that row's ancestors and centres it.

Pipe mode means `:r !xless %` and `:%!xless` also work, with no extra code.

---

## 12. Testing and CI

- **All tests are inline `#[cfg(test)]` unit tests** (90 in total) inside
  these modules: `xmlparser`, `flatxml`, `viewer`, `lineprinter`, `search`,
  `path`, `edit` and `config`.
  - There's no `tests/` directory, no golden fixtures and no TUI or
    event-loop tests.
  - `indoc` is a dev-dependency but isn't used.
  - The interactive behaviour in §15 was checked with a pty driver against
    the release binary. That driver isn't part of the repo.
- **Tests build documents in memory** with `Source::Owned`, so they never
  touch mmap or the terminal.
- **CI** (`.github/workflows/ci.yml`) runs build + test on ubuntu-latest and
  macos-latest (x86_64), plus `clippy -D warnings` and `fmt --check`. The
  MSRV declared in `Cargo.toml` is 1.75, but CI only tests `stable`.

---

## 13. Cost model (where time and memory go)

| Operation | Cost | Measured: 15 MB / 150 MB* |
|---|---|---|
| Open file | mmap + O(file) UTF-8 validation + one O(file) eager parse (no lazy phase 2) | 0.12 s / 0.93 s |
| Resident memory once open | mmap'd source + `Vec<Row>` (36 B/row) + `Vec<Attr>` | 67 MB / 645 MB |
| Redraw | O(screen rows), each row a small `String` allocation; header path O(depth) via `PathCache` | ~0 ms (tables built on first use: G ≈ 8 / 64 ms) |
| Move / scroll | O(rows stepped); collapsed subtrees skipped in O(1) | ~0 ms |
| Search jump | O(rows to next match), one allocation per row | ~50 ms |
| First edit on a file opened via mmap | O(file) copy into a `Vec` (the mmap is released) | — |
| Each committed edit / undo / redo | O(file) splice + **O(file) full reparse** | 0.12 s / 1.1 s; peak 84 MB / 800 MB |
| Rejected edit | two O(file) parses (try, then rebuild) | — / 2.2 s |
| Save | O(file) write + fsync + rename | 0.07 s / 0.54 s |
| Pipe mode | parse + streamed pretty-print | 0.31 s, 68 MB / 2.75 s, 645 MB |

\* Release build, 80×24 pty.
- The 15 MB file is a real 82k-`<book>` catalogue (1.4 M rows).
- The 150 MB file is the same content repeated 10× under one root
  (14 M rows, all 820k books siblings of the root).
- Before the §15 pass, the 150 MB numbers were: 1.03 GB resident, 1.9 GB
  peak after an edit, 1.45 s per edit, and 1.24 GB for pipe mode.

Memory is dominated by the row index: about 14 M rows × 36 bytes ≈ 500 MB
for 150 MB of this shape, which has short elements and roughly 11 bytes of
source per row. Shrinking it further would mean squeezing `Value` or
`depth`, or adding a lazy second parsing phase (README Architecture §8.4).

---

## 14. Design vs. implementation drift

`README.md` mixes design intent with status notes. These are the places
where the code does something different, so read the code rather than the
README here:

| README says | Code does |
|---|---|
| Patch overlay over the original mmap'd bytes; subtree-scoped reparse/splice (Editing §2–§3) | Copies the whole buffer into an owned `Vec` on the first edit and re-parses the whole document per commit (the Editing status note acknowledges this) |
| Streaming `:w` that walks the original bytes plus a sorted patch list (Editing §4) | Writes the whole owned buffer to a temp file, then renames it |
| Two-phase parse: structural pass, then lazy attribute and whitespace detail (Arch §8.4) | One eager pass |
| `--show-whitespace` flag (Arch §3) | Not implemented (`xml:space="preserve"` handling *is*) |
| `truncatedstrview.rs` ported for horizontal scrolling (Arch §6) | Hard truncation only |
| Per-token highlighting (attribute name/value colours) (Arch §6) | One colour per row |
| `:` command mode via rustyline (Roadmap M2) | Hand-rolled `read_line` |
| `--focus-pos` flag (Roadmap M4, `viewer.rs` doc comment) | Only `--focus-line` exists |
| Attribute-value yank target (Arch §7.3) | Not implemented |
| Large-file budget: peak memory 2–4× file size (Arch §8.6) | About 4.3× once open, about 5.3× at the peak of an edit (150 MB file, §13) |

Code comments that pointed at the old split docs (`ARCHITECTURE.md §x`,
`EDITING.md`, `PLAN.md`, `VIM_INTEGRATION.md`) now point at the matching
`README.md` sections.

---

## 15. Fixed issues and large-file verification

Each issue below was found by reading the code and confirmed against the
binary:
- a pty driver;
- an 80×24 terminal;
- a throwaway `$HOME`.

Each one is now fixed and has a regression test, except where noted.

| # | Issue | Fix | Test |
|---|---|---|---|
| 1 | `cat f.xml \| xless` exited after one frame: keys were read from the exhausted stdin pipe | `main.rs` points fd 0 at `/dev/tty` before raw mode | pty only |
| 2 | A mouse click focused the row *below* the one clicked (the header row wasn't counted) | `handle_mouse` subtracts `HEADER_ROWS`; clicks on the header or status bar are ignored | pty only |
| 3 | Edit → `:w` → `u` → a different edit showed as clean, so `q` silently dropped the edit | Dirty check compares patch ids (§10.2) | `test_dirty_after_save_undo_and_different_edit`, `test_undo_redo_back_to_saved_state_is_clean` |
| 4 | Every edit, undo or redo re-expanded the whole document | Collapse offsets are remapped through the edit (§10.2) | `test_collapse_state_survives_*`, `test_deleted_collapsed_element_*`, `test_rejected_edit_restores_*` |
| 5 | Pipe mode flattened newlines and tabs, re-quoted `'` as `"` (malformed when a value contains `"`), and dropped `xml:space="preserve"` whitespace | Full-fidelity printers (§7.2), preserve scopes in the parser (§5) | `test_pretty_printed_*`, `test_whitespace_kept_inside_xml_space_preserve`, … |
| 6 | `NG` in Compact mode could focus a hidden closing tag, and so could toggling `m` while on one | `Viewer::displayable_row` (§6) | `test_jump_to_closing_row_*`, `test_toggling_to_compact_*` |
| 7 | The header path re-walked siblings every frame (45–90 ms per key at 150 MB) | `PathCache` (§9.2) | `test_cache_matches_fresh_builds_across_many_lookups` |
| 8 | `:w` through a symlink replaced the link with a regular file and **left the real file unedited**; permissions were lost | `save` resolves symlinks and copies permissions (§10.3) | `test_save_writes_through_symlink_and_keeps_permissions` |
| 9a | `scroll_up` clamped against the full terminal height, allowing 2 rows less bottom padding than `--scrolloff` | Uses `content_height()` | `test_scroll_up_keeps_scrolloff_padding_at_bottom` (fails on the old code) |
| 9b | The help pager ignored `settings.json` (j/k/g/G/q were hardcoded) | Looks up the configured keymap, plus arrows/PgUp/PgDn/Home/End/Esc | — |
| 9c | Stale comments: the leading-`0` comment, the `lineprinter.rs` header, 14 references to the old doc files | Corrected | — |
| 10 | Found during benchmarking: `n` on the document's only match said "no matches" | `find` returns the sole match, and the status bar says so | `test_find_from_the_only_match_returns_it` |
| 11 | Found during benchmarking: `xless big.xml \| head` panicked on EPIPE | Streaming writer; `BrokenPipe` is treated as a normal exit | pty only |
| 12 | Found during benchmarking: 150 MB took 1.03 GB once open and 1.9 GB at the peak of an edit | 36-byte rows with derived links (§4.2), old index freed before the reparse (§10.2), streamed pipe output | `test_row_is_compact`, `test_derived_links_match_brute_force` |

**Large-file checks** (all passed) on the 15 MB and 150 MB files:
- **Pipe mode** gives the same element tree as the input (tags, attributes
  and trimmed text and tails), and is byte-identical to the pre-fix
  binary's output.
- **A 150 MB edit → undo → redo → `:w`** produces a file identical to the
  original except for exactly the renamed tag pair.
- **A rejected edit at 150 MB** leaves the file untouched and keeps the
  collapse state.

**Known remaining limits:**
- **Edits reparse the whole document**: about 1.1 s at 150 MB, and 2.2 s
  when the edit is rejected.
- **Memory is about 4–5× file size** for element-dense XML.
- **Saving doesn't preserve file ownership.**

---

## 16. Extending xless — where to make common changes

- **New key-triggered action:**
  1. Add a variant to `config::AppAction` and an entry to `config::ACTIONS`
     (name, default keys, help section, description). The defaults, the JSON
     config and `:help` pick it up automatically.
  2. Handle it in `App::handle_key`'s `match`.
  3. If it's pure navigation, add a `viewer::Action` variant and check both
     exclusion lists in `perform_action`.
- **New yank target:** add a `config::YankTarget` variant and a `YANK_TARGETS`
  entry, then handle it in `App::handle_prefixed_key`. Rendering helpers live
  in `lineprinter.rs`.
- **New `:` command:** add a branch to `App::command_mode`, and update the
  hardcoded command list in `build_help_text`.
- **New row kind:**
  - `flatxml::Value`;
  - the parser event mapping;
  - `lineprinter::{render_row, write_full_fidelity_row}`;
  - `highlighting::style_for`;
  - `search::searchable_text`;
  - `path` leaf labels;
  - `edit::node_full_span`'s delimiter arithmetic.

  The `match`es are exhaustive, so the compiler will point at each one.
- **New edit command:** compute `(Range<usize>, Vec<u8>)` spans plus a
  `focus_hint`, then call `App::commit_edit`. Validation, rollback, undo and
  dirty tracking come for free. Don't mutate `doc.source` or `doc.flat`
  directly.
- **Adding screen chrome rows:** bump `screenwriter::HEADER_ROWS`. Both
  `viewer.reserved_rows` (set in main) and `App::handle_mouse`'s row
  mapping are derived from it.
- **Anything that replaces `viewer.doc`:** clear `App`'s `path_cache` and
  `search.current_match` (see `App::commit_edit`). Row indices aren't
  stable across a reparse.
- **New per-row data:** think about the 36-byte budget
  (`test_row_is_compact`). Prefer deriving it from row order, or keeping it
  in a side table (the way `attrs` and `PathCache` do), over adding a field
  that costs 14 M × size at 150 MB.
