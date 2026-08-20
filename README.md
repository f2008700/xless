# xless

`xless` is a standalone, Rust, command-line XML tool: run `xless
somefile.xml` and it opens directly into a syntax-highlighted,
collapsible tree view with vim-inspired navigation, regex search, *and*
vim-style modal editing (rename tags, edit attributes/text, insert/delete
elements, save back to disk) — a one-stop `jless`/`less`-plus-editor for
XML. Built to stay fast on large files (target: 100–200MB XML) via
mmap'd input and lazy parsing rather than loading everything into memory
up front. Also driveable from inside vim/neovim, but that's a convenience
layer on top — the standalone binary is the whole product on its own.

## Status

Implemented: viewing (navigation, collapse/expand, Line/Compact mode,
line numbers), regex search, yank/paste through the system clipboard,
`:` command mode, mouse support, editing (rename/delete/insert/edit
content, undo/redo, save) with a validate-and-rollback safety net against
ever saving invalid XML, and configurable keybindings
(`~/.xless/settings.json`, see [Keybinding configuration](#keybinding-configuration)
below). See the [Roadmap](#roadmap) section's status notes for the
precise cut line. Not yet packaged for distribution (no published crate
or platform packages yet — build from source with `cargo build --release`).

## Install (from source)

```sh
git clone <this repo> && cd xless
cargo install --path .
```

## Contents

This file is the single source of documentation for the project (design,
editing model, vim integration, keybinding config, and roadmap all live
here rather than split across a `docs/` directory, to keep everything in
one place):

- [Architecture](#architecture) — the design (derived from studying
  [`jless`](https://jless.io)) and the large-file performance strategy.
- [Editing model](#editing-model) — how in-place editing works.
- [Vim integration](#vim-integration) — driving xless from vim/neovim
  (see also [`contrib/vim/README.md`](contrib/vim/README.md) for the
  plugin's own install instructions).
- [Keybinding configuration](#keybinding-configuration) — the
  `~/.xless/settings.json` override file.
- [Roadmap](#roadmap) — milestone plan and current status.

---

## Architecture

`xless` is `jless` for XML: a terminal pager that shows a syntax-highlighted,
collapsible tree view of an XML document, with vim-style movement and
regex search. This section records what was learned from reading the
`jless` source (~11.3k lines of Rust) and how those ideas translate — or
don't — to XML.

### 1. What jless actually is, structurally

jless is not "a JSON pretty-printer with a TUI bolted on." Its core insight
is a single flattened data structure, `FlatJson`, that both the renderer and
the navigation logic operate on:

```rust
pub struct FlatJson(
    pub Vec<Row>,   // one entry per visible-or-collapsible line
    pub String,      // canonical, freshly re-rendered pretty-printed text
    pub usize,       // max nesting depth
);

pub struct Row {
    pub parent: OptionIndex,
    pub prev_sibling: OptionIndex,
    pub next_sibling: OptionIndex,
    pub depth: usize,
    pub index_in_parent: usize,
    pub range: Range<usize>,          // byte range into the String above
    pub key_range: Option<Range<usize>>,
    pub value: Value,                  // Null/Bool/Number/String/Open{..}/Close{..}
}
```

Two things stand out that are easy to miss on a skim:

1. **The parser doesn't preserve the original file text.** `jsonparser.rs`
   both flattens the tree *and* re-renders a canonical, 2-space-indented
   pretty-printed `String` as it goes. Every `Row.range` is a byte range
   into that *regenerated* string, not the original input. This is why
   `jless` can cheerfully reformat minified JSON, and why search/yank/path
   building never have to deal with the original file's whitespace quirks.
   `yamlparser.rs` does the same thing for YAML — it parses with
   `yaml_rust` and then re-renders through the *same* JSON-shaped
   `Value`/`Row` model, i.e. YAML is lossily normalized into "JSON but
   with fancier keys." That normalize-to-canonical-form move is the
   pattern we should copy, but XML can't reuse the *same* `Value` shape
   (see §3).

   **xless makes a different choice here, though** — see §4 and §8.
   jless's inputs are small enough (KB–MB JSON) that materializing a
   second full copy of the file as a re-rendered `String` is free. xless's
   stated target is XML files up to 100–200MB, where a second full-size
   buffer is a real cost, not a rounding error. `Row.range` in xless
   points into the **original source bytes** (mmap'd, not copied), and
   reformatting (indentation, whitespace cleanup) happens per-row at
   render time instead of once up front. This is a deliberate divergence,
   not an oversight — noted here so it's clear from the first read that
   xless's `Row` doesn't carry a "pretty-printed companion string" the
   way `FlatJson` does.

2. **Everything downstream is format-agnostic.** `viewer.rs` (navigation
   state machine), `input.rs` (raw terminal input), `terminal.rs` (ANSI
   color/style abstraction over termion), `truncatedstrview.rs` (horizontal
   line truncation/scrolling), `screenwriter.rs` (status bar, line numbers,
   frame composition) — none of these know anything about JSON specifically.
   They operate on `Row`/`FlatJson`-shaped data. Only `flatjson.rs` itself,
   `jsonparser.rs`/`jsontokenizer.rs`/`jsonstringunescaper.rs`,
   `yamlparser.rs`, and parts of `lineprinter.rs`/`highlighting.rs` know
   what JSON *is*.

That split is the single most important fact for scoping this port: **most
of jless's line count is reusable almost as-is.** See §6 for the file-by-file
reuse plan.

### 2. Navigation model (kept as-is)

`FlatJson` supports two notions of "next":

- `next_visible_row` / `prev_visible_row`: literally the next row, skipping
  over the body of a collapsed container.
- `next_item` / `prev_item`: the next *sibling-level* thing, i.e. it also
  skips `CloseContainer` rows. This is what backs `j`/`k`-adjacent "move to
  next value" commands.

Collapse state lives directly on the `Value::OpenContainer`/`CloseContainer`
pair (`collapsed: bool`, kept in sync on both halves by `FlatJson::collapse`/
`expand`/`toggle_collapsed`). `first_visible_ancestor` walks up `parent`
links to find where a collapsed ancestor would redirect the cursor.

This model — index-based double-linked tree flattened into a `Vec`, with
open/close as two rows referencing each other via `pair_index()` — maps onto
XML with no structural changes needed: an XML element with children becomes
an `OpenElement`/`CloseElement` row pair exactly like `OpenContainer`/
`CloseContainer`. **We should port `flatjson.rs`'s traversal methods
almost verbatim**, changing only the `Value` enum they close over.

### 3. The XML data model (`flatxml.rs`, new)

This is the one piece jless can't hand us for free, because XML's grammar
doesn't look like JSON's:

- JSON has exactly two container kinds (object, array) and values are
  either a container or a primitive.
- XML elements have **three** things at once: a tag name, an *ordered list
  of attributes* (name/value string pairs, not nested structure), and
  *content*, which is a mixed sequence of child elements, text, comments,
  CDATA, and processing instructions. "Mixed content" (text interleaved
  with child elements, e.g. `<p>Hello <b>world</b>!</p>`) has no JSON
  analog at all.
- A well-formed document has exactly one root element, but a *prolog*
  (XML declaration, DOCTYPE, comments/PIs) can precede it and *misc*
  (comments/PIs) can follow it — similar in shape to jless's support for
  multiple concatenated top-level JSON values (see `MULTI_TOP_LEVEL` test
  in `flatjson.rs`), just with a fixed one-element-in-the-middle
  constraint instead of an arbitrary list.

Proposed `Value` enum:

```rust
pub enum Value {
    Text,                    // character data
    CData,                   // <![CDATA[ ... ]]>
    Comment,                 // <!-- ... -->
    ProcessingInstruction,   // <?target ...?> (incl. the XML declaration)
    DocType,                 // <!DOCTYPE ...> (opaque, single row, prolog only)

    EmptyElement {
        attributes: Range<Index>,   // slice into a side-table of AttrRows (see below)
    },
    OpenElement {
        collapsed: bool,
        attributes: Range<Index>,
        first_child: Index,
        close_index: Index,
    },
    CloseElement {
        collapsed: bool,
        last_child: Index,
        open_index: Index,
    },
}
```

`Text`/`CData`/`Comment`/`ProcessingInstruction`/`DocType` all behave like
jless's primitives (`Null`/`Boolean`/`Number`/`String`) for navigation
purposes — `is_container()` is false, they're never collapsible, `next_item`
just steps over them.

**Attributes are metadata, not tree children.** Modeling `<user id="7"
active="true">` by turning `id`/`active` into fake child rows would break
the clean "container has children, primitives don't" invariant every
traversal function relies on, and would make `w`/`b` (move until depth
change) and sibling-focus commands behave surprisingly. Instead, an
element's attributes are rendered *inline in the opening-tag line*
(reusing `truncatedstrview.rs` for horizontal scroll/truncation when the
tag line is too long — that module is already generic over "a string too
wide for the terminal," it doesn't need to change). Each attribute's byte
range in the canonical text is still recorded (in a small side table
`Vec<AttrRow>` indexed by the `attributes: Range<Index>` above) purely so
search-highlighting and `:yank attribute` can address individual
attributes without making them first-class tree nodes. This is a deliberate
divergence from "one row per visible thing," and is flagged as such in
§7 (open questions) — if in practice users want to collapse/search
attribute-heavy tags line-by-line, promoting attributes to real rows in
**Line mode only** (see §5) is the natural extension, and the side-table
already gives us the byte ranges to do that without re-parsing.

**Insignificant whitespace.** Pretty-printed XML commonly has text nodes
that are pure indentation whitespace between tags (`<a>\n  <b/>\n</a>`).
Unlike JSON, XML doesn't have a "this whitespace doesn't count" rule built
into the grammar — it's a per-vocabulary convention (`xml:space="preserve"`
opts out). Default behavior: the parser drops whitespace-only text nodes
that are *not* inside an `xml:space="preserve"` scope, mirroring how a
browser DOM viewer would show it, with a `--show-whitespace` flag to keep
them as explicit `Text` rows for people debugging exact serialization.

### 4. Parsing (`xmlparser.rs`, new)

Use [`quick-xml`](https://docs.rs/quick-xml) as the tokenizer, in pull-parser
(`Reader::read_event`) mode, analogous to how `jsonparser.rs` drives the
`logos`-generated `JsonToken` lexer by hand. `quick-xml` is well-maintained,
has no required allocation-heavy DOM step, supports namespaces as an
opt-in, and — importantly — gives byte offsets into the *original* input
for every event (`Reader::buffer_position`).

That last point turns out to matter twice over: it's both how xless avoids
the second-full-copy memory cost flagged in §1 for large files, and how it
gets a feature jless didn't need — **mapping an editor's cursor line in
the *original* file to a row in the viewer** (see [Vim integration](#vim-integration))
for free.

Concretely: `Row.range` is a byte range into the mmap'd **original**
file, populated straight from `quick-xml`'s position tracking
(`Reader::buffer_position`) as each row is created — not into a second,
regenerated pretty-printed string the way jless's `Row.range` is. This
means:

- There's no `original_pos` field needed — `row.range.start` *is* the
  original-file position, always. "Which row is line 42 of the file on
  disk in" is a direct lookup, not a second source map to keep in sync.
- Rendering (`lineprinter.rs`) is responsible for presenting each row's
  slice of the *original* bytes nicely (adding indentation for the row's
  depth, trimming redundant original whitespace around `=`/`<`/`>`) at
  draw time, rather than reading already-reformatted text. This is more
  work per render call than jless's approach, but it's the single biggest
  lever for the 100–200MB performance target — see §8 for the full
  reasoning and the memory numbers that motivate it.
- One consequence worth flagging early: byte-identical round-tripping of
  *unedited* regions becomes easy to get for free in the editing model
  (see [Editing model](#editing-model)) — "unchanged" literally means "no
  patch covers this range of the original file," which is exactly the
  representation a streaming save wants anyway.

Namespaces: treated as opaque text. `xmlns:foo="..."` is just another
attribute; `<foo:bar>` is just a tag name string. No namespace-URI
resolution in v1 — this keeps the parser simple and, more importantly,
keeps search/yank/path-building operating on exactly the text the user
typed, which is what a "viewer," as opposed to an XML-aware processor,
should do. (`jless` makes the analogous simplicity trade-off: YAML anchors/
aliases are explicitly unsupported — see `Yaml::Alias` in `yamlparser.rs`
returning an error — rather than half-modeled.)

Malformed/non-well-formed input: v1 targets well-formed XML only, same
scoping choice jless makes for JSON/YAML (garbage in -> a parse error
message and exit, not a best-effort partial render). A later milestone can
explore a lenient "HTML-soup" mode; explicitly out of scope for v1 (see
[Roadmap](#roadmap)).

### 5. Two viewing modes (kept as a concept, renamed) — implemented

jless's `Mode::Line` vs `Mode::Data` toggle (`m` key) controls how much
JSON punctuation is elided for readability vs. shown for fidelity. The XML
equivalent, same toggle key, **implemented** in `viewer.rs`/`lineprinter.rs`:

| | **Line mode** | **Compact mode (default)** |
|---|---|---|
| Attributes | inline in the opening tag (see note below) | inline in the opening tag, truncated if needed |
| Empty elements (`<a></a>` — no content, not self-closed) | `<a>` / `</a>` on two lines | `<a/>` |
| Closing tags | always their own row, always visible | **never** independently visible — elided unconditionally, matching jless's actual Data-mode behavior ("closing braces ... are elided," not just for empty containers — an earlier draft of this table got that nuance wrong; corrected once the implementation made the difference concrete) |
| Element preview when collapsed | `<tag …>…</tag>` | `<tag …>…</tag>` (same in both modes — collapsed rows never expose the mode distinction, since the close row isn't independently visible while collapsed either way) |
| Whitespace-only text nodes | never shown | never shown |

Corrected mechanism, now that it's built: the mode isn't just a rendering
switch, it changes which *traversal function* movement uses —
`next_visible_row`/`prev_visible_row` (Line: closing tags are real,
independently-visible rows) vs. `next_item`/`prev_item` (Compact: closing
tags are always skipped) — exactly mirroring how jless's own Data mode
works, not an XML-specific invention. `viewer.rs`'s module doc comment has
the full account, including why "one attribute per physical line" (this
table's original Line-mode idea) was dropped in favor of keeping
attributes inline in both modes: it would have meant one logical `Row`
spanning multiple *terminal* lines, breaking the one-row-one-line
invariant `screenwriter.rs` depends on. Per-attribute rows remain a
possible future direction (§7.2), just not via multi-line wrapping of the
existing row.

### 6. File-by-file port plan

Legend: **keep** = copy with only renaming/import fixes; **adapt** = same
shape, needs XML-specific branches; **rewrite** = new file, jless version is
a reference/precedent, not a base.

| jless file | LoC | xless treatment | Notes |
|---|---:|---|---|
| `terminal.rs` | 419 | **keep** | ANSI color/style/cursor abstraction over termion. Zero JSON knowledge. |
| `types.rs` | 38 | **keep** | `TTYDimensions`. |
| `truncatedstrview.rs` | 1125 | **keep** | Generic "string too wide for the line" scrolling/truncation logic. |
| `input.rs` | 256 | **keep** | Raw terminal input + SIGWINCH handling via termion/signal-hook/libc. |
| `screenwriter.rs` | 572 | **adapt** | Frame composition, status bar, line numbers — swap `JsonViewer`→`XmlViewer` types, otherwise structural. |
| `viewer.rs` | 2292 | **adapt** | `Action` enum and `perform_action` state machine are format-agnostic; swap `FlatJson`→`FlatXml`, keep almost all movement logic. Mode enum gains XML semantics from §5. |
| `search.rs` | 582 | **adapt** | Core wrap-around/expand-collapsed-containers logic is unchanged, but *what* gets regexed changes — see §8.5, since there's no single canonical string to scan the way jless has. |
| `lineprinter.rs` | 1984 | **rewrite** (structure kept, content new) | Needs full new set of per-`Value` rendering branches (tag/attrs/text/comment/CDATA/PI) per §5's mode table. |
| `highlighting.rs` | 234 | **rewrite** | New palette: tag name, attribute name, attribute value (quotes), text content, comment, CDATA, PI/decl. |
| `flatjson.rs` | 1242 | **rewrite** (traversal logic ported, `Value`/`Row` new) | Traversal methods (`next_visible_row`, `next_item`, `expand`/`collapse`, `first_visible_ancestor`) port near-verbatim onto the new `Value` enum from §3. `build_path_to_node` is replaced by XPath generation (§7). |
| `jsonparser.rs` + `jsontokenizer.rs` + `jsonstringunescaper.rs` | 821 | **rewrite** | Replaced by `xmlparser.rs` built on `quick-xml` (§4). |
| `yamlparser.rs` | 516 | n/a | Not applicable — no second input format in v1. Its "normalize into the shared row model" *pattern* is reused for the design in §3, but no code carries over. |
| `app.rs` | 985 | **adapt** | Top-level event loop, multi-key command buffering (counts, `g`/`z`/`y`/`p` prefixes), `:` command mode via rustyline. Structure is unchanged; the yank submenu's `ContentTarget` enum gets XML-shaped variants (§7). |
| `options.rs` | 93 | **adapt** | Same clap-derived `Opt` shape; new flags per §4/§5 (`--show-whitespace`, format is always XML so `--json`/`--yaml`-style flags drop out, replaced by nothing — one format, one parser). |
| `main.rs` | 136 | **adapt** | Same shape: read input (mmap, §8.1, not `read_to_string`), if stdout isn't a tty print a pretty-printed rendering and exit (needed for both `xless file.xml | ...` pipelines and the `:r !xless %` vim recipe), else set up raw/alternate-screen termion terminal and run the event loop. |

Rough result: ~2400 LoC ports with near-zero conceptual change, ~1900 LoC
adapts (same shape, new domain branches), ~2600 LoC is genuinely new
(parser + data model + line rendering + highlighting). That's a much
smaller lift than "rewrite an 11k-line TUI from scratch," which is the
whole point of studying jless first.

### 7. Open design questions to resolve early (not blocking a v1 skeleton)

1. **Path/query language.** jless builds jq-style paths (`.foo[3].bar`,
   `["foo"][3]["bar"]`) via `build_path_to_node`. For XML the natural
   analog is **XPath**: `/root/child[2]/@attr`, `/root/child[2]/text()`.
   Sibling indexing should probably follow XPath convention (1-based,
   *and* only counted among same-tag siblings — `child[2]` means "the 2nd
   `<child>` among its siblings," not "the 2nd child overall") since that's
   what every XML user's muscle memory expects from `xmllint --xpath` /
   browser devtools "Copy XPath." This is a bigger semantic difference
   from jless's index-in-parent-among-all-siblings than it looks like at
   first glance and deserves its own small design note before
   implementation (`index_in_parent` in `Row` today counts all siblings;
   XPath needs a same-tag-only count, so either a second field or a
   computed pass).
2. **Attributes as rows.** Flagged in §3 — v1 renders them inline; decide
   after real-world use whether attribute-heavy documents (SOAP/config
   XML with 10+ attributes per tag) need per-attribute rows in Line mode.
3. **Yank targets.** jless's `y` submenu offers pretty-printed value,
   one-line value, string contents, key, and three path flavors. XML
   analog: pretty-printed subtree, one-line/minified subtree, text content
   (concatenated descendant text, like `.textContent`), tag name, XPath —
   plus a plausible new one, **attribute value**, since attributes aren't
   separately selectable any other way (tie into whichever attribute the
   cursor's horizontal sub-position is over, using the side-table from §3).
4. **Editing scope — resolved, implemented.** Went with the constrained
   direction: structural commands (rename tag, edit text/raw-attribute-
   blob, insert/delete element) rather than free-text editing of arbitrary
   regions, per the [Editing model](#editing-model) section's §1. Attribute
   values are edited as one raw blob per element, not individually
   addressable fields — a further simplification from even the
   "constrained" plan, since attributes still aren't separate rows (point
   2 above remains open/unimplemented).

### 8. Large-file performance strategy (100–200MB inputs)

Stated target: source XML files up to 100–200MB, opened and made
interactively navigable fast, with no multi-second stalls during normal
use (scrolling, expanding/collapsing, searching). This is a hard
constraint on the design, not a later optimization pass — a few choices
in §1–§4 are already written the way they need to be *because* of this
section; this is where the "why" and the numbers live.

For scale: jless's inputs are typically KB–MB JSON, and its `main.rs`
reads the whole file into a `String` and then, per §1, builds a *second*
full-size regenerated `String` during parsing. Doing that at 200MB would
mean ~400–500MB of heap allocation (accounting for re-indentation growth)
before the first frame is even drawn, and jless's own `Row` (§ above,
`OptionIndex` + `Range<usize>` + `Value` fields, effectively several
8-byte words per row) sized for a file with orders of magnitude fewer
rows. None of this is a criticism of jless — it's correctly scoped for
its actual inputs — it's just not the right default to inherit unmodified
here.

1. **mmap the source, don't `read_to_string` it.** Use `memmap2` to map
   the file read-only instead of copying it into a `String`. The parser
   (§4) and every later lookup (rendering, search) read from the mapped
   bytes. Trade-off: lose `String`'s "always valid UTF-8" guarantee for
   free; mitigate by doing one bulk UTF-8 validation pass up front (phase
   0 of §8.4) and using validated `&str` slices into the map afterward —
   the same pattern tools like ripgrep use over large inputs.

2. **No second full-size canonical-text buffer.** Already decided in §1/
   §4: `Row.range` points into the original mmap'd bytes, not a
   regenerated pretty-printed string. `lineprinter.rs` reformats
   (indentation, whitespace cleanup) per row, at render time, for
   whichever handful of rows are actually on screen — not for the whole
   file up front. This is the single biggest memory lever available.

3. **Compact `Row` representation.** Use `u32` (not `usize`) for indices
   (parent/sibling/pair-index/etc.) and for byte offsets — 4 billion rows
   and a 4GB file are both far past the stated 100–200MB target, so this
   costs no real headroom and roughly halves `Row`'s size on a 64-bit
   target. Rough sizing: at a density of very roughly one `Row` per
   50–150 bytes of XML (element/text/attribute boundaries), a 200MB file
   is on the order of 2–4M rows — halving `Row` is the difference between
   very roughly 150–300MB and 300–600MB just for the index. This is
   mechanical and low-risk (no rendering-logic impact), so it should be
   the default representation from M0 (see [Roadmap](#roadmap)), not a
   later retrofit.

4. **Two-phase parse: fast structural skeleton, detail on demand.**
   Phase 1 (blocking, must be fast): one pass over the mmap'd bytes
   building only what navigation needs — `Value` variant, `depth`,
   parent/sibling/pair indices, byte range. No attribute value parsing,
   no whitespace-significance analysis. This should be close to I/O-bound
   at 100–200MB, not CPU-bound. Phase 2 (lazy, memoized): attribute
   ranges and "is this text node whitespace-only" classification are
   computed the first time a row is actually rendered or searched, not
   up front. Opening a 200MB file's first screen shouldn't wait on
   parsing attributes belonging to element #2,000,000.
   - Fallback if phase 1 alone isn't fast enough in practice (to be
     measured against real large fixtures once M0 exists — this is a
     hypothesis to validate, not a promise): run phase 1 on a background
     thread, render the first screen from a partial index plus a
     "still indexing…" status-bar message (reusing jless's existing
     `MessageSeverity`-style mechanism in `screenwriter.rs`), and let
     navigation past the indexed frontier block briefly until it catches
     up. Sequencing this as a fallback rather than a day-1 requirement
     avoids taking on threading complexity before knowing it's needed.

5. **Search without a second scan-friendly buffer.** jless's `search.rs`
   regexes over its one canonical string. Under §8.2 there is no such
   buffer. Two candidate approaches, to be decided from real benchmarks
   in M2 rather than guessed now: (a) regex the mmap'd original bytes
   directly — a 200MB scan is typically sub-second with the `regex`
   crate's memchr-accelerated literal prefixes, but "typically" needs
   verifying against real fixtures, not assumed — then map a match's byte
   offset back to a row via binary search over a sorted `range.start`
   index; or (b) if raw-byte search is too noisy (matching inside tag
   syntax/entities users don't care about), restrict search to specific
   row kinds (text, attribute values, tag names) and iterate rows instead
   of regexing a blob. Recorded here so this isn't a "just port
   search.rs" afterthought when M2 starts.

6. **Budget to validate against (planning targets, not guarantees).** For
   a 200MB input: peak resident memory on the order of 2–4x file size
   (dominated by §8.3's row index, since the source itself is mmap'd, not
   resident-counted the same way a heap copy is), and first-screen
   interactivity in low single-digit seconds worst case, ideally
   sub-second. [Roadmap](#roadmap)'s M0 exit criteria should include
   measuring a real 100–200MB fixture against these numbers, not just
   checking output correctness on small files.

### 9. Standalone-first, and editing (not just viewing)

Two scope decisions, best-effort interpretation of stated requirements —
flagged for confirmation rather than treated as fully settled:

**Standalone-first.** `xless <file>` must be the *whole product* on its
own — a full interactive, vim-keybinding-driven session — with zero
dependency on vim. This was already implicit in following jless's
`main.rs` contract (§6, [Vim integration](#vim-integration)'s §1:
`xless file.xml` grabs its own tty and runs, full stop; being launchable
*from* vim via `:!`/`:terminal` is just a consequence of behaving like a
normal terminal program, not a separate integration path). Restated
explicitly here so it's an intentional property of `main.rs`/§4/§8, not
an accident of following jless too closely: vim integration (see
[Vim integration](#vim-integration)) is a convenience layer on top of a
standalone tool, never a prerequisite for using it.

**Editing, not just viewing.** jless is deliberately read-only — its
README pitches it as a replacement for "less, jq, cat, and your editor"
but stops short of *being* the editor. The requirement here goes further:
xless itself should support modal (normal/insert/command, vim-like)
*editing* of the XML — renaming tags, changing attribute/text values,
inserting/deleting elements, saving back to disk — not just navigation.
This is a genuine, substantial addition on top of everything jless's
codebase gave us a head start on (§1–§7 are still exactly the navigation/
rendering foundation; editing is a new layer above them). It also
interacts directly with §8: naive whole-buffer text editing is fine for
small files but not for a 200MB one, so the edit representation has to be
patch-based from the start rather than "make it work small, optimize
later." Full design: [Editing model](#editing-model).

---

## Editing model

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
reasoned about here in one place. This section assumes
[Architecture](#architecture) §3 (data model), §4 (parsing, `Row.range`
into mmap'd original bytes) and §8 (100–200MB performance target) as
given — the edit model is designed *because of* §8, not despite it:
whole-buffer text editing (what a normal text editor does) is fine at
small sizes and a bad idea at 200MB, so the representation below is
patch-based from the start.

**Scope, as shipped**: the constrained-structural-operations direction
[Architecture](#architecture) §7.4 flagged as the recommended default is
what got built, with one simplification from the original per-attribute
vision: attributes are edited as one raw text blob (` id="1"
active="true"`) rather than individually. See [Architecture](#architecture)
§7.2 for why (attributes aren't individually addressable rows) —
promoting them to per-attribute editing remains a possible future
direction, not a blocker for having a genuinely useful `i` today.

### 1. Modes and commands (as implemented, `app.rs`)

Normal mode gains, on top of every navigation/collapse command ported
from jless ([Architecture](#architecture) §2/§6):

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
section originally sketched it — copy in xless, paste in your editor, or
vice versa, all work.

### 2. Why not just edit the in-memory row/text buffers directly

`Row.range` values are byte ranges into the mmap'd original file (or, for
small files under some threshold, a `String` — see §3). Splicing text into
the *middle* of that buffer would invalidate every subsequent row's
range in the whole file — an insertion at byte 1000 shifts every range
after it. Re-deriving all of them on every keystroke is exactly the
whole-file-rescan cost §8 exists to avoid. So edits never mutate the
source buffer or the `Row` ranges in place; they're tracked as an overlay
instead.

### 3. Patch overlay

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

### 4. Saving

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

### 5. Keeping edits well-formed — as implemented

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

### 6. Interaction with vim launch — resolved

[Vim integration](#vim-integration) §3's temp-file trick (open a scratch
copy when the vim buffer has unsaved changes, so xless shows your actual
edits) was designed when xless was view-only, before editing existed. The
shipped `contrib/vim/plugin/xless.vim` handles the guard this section
originally flagged as unresolved: when it writes a scratch copy, it
prints an explicit `echom` warning naming the temp path and stating that
edits made inside that xless session won't flow back into the vim
buffer. It doesn't force the session read-only — xless's own status bar
already shows whichever path it's actually operating on (the temp file's,
in that case, not your real filename), which combined with the one-time
warning was judged sufficient rather than removing the ability to edit a
just-unsaved-buffer's content entirely. Revisit if this turns out to be
confusing in practice.

---

## Vim integration

**xless is standalone-first** ([Architecture](#architecture) §9): `xless
somefile.xml` on its own, with no vim involved at all, is the whole
product — a full interactive session with vim-style navigation *and*
editing (see [Editing model](#editing-model)). Nothing here is a
prerequisite for using xless. This section covers driving it *from* vim
specifically, because that's an explicit, important target workflow, not
because xless needs vim to be useful. It's split into what works "for
free" because of decisions already made in [Architecture](#architecture),
and what needs a small shipped plugin.

### 1. Works for free, if we keep jless's terminal contract

jless behaves exactly like `less`: it detects whether stdout is a real
terminal, and if not, it just prints and exits (`main.rs`:
`isatty::stdout_isatty()` check → `print_pretty_printed_input` then
`std::process::exit(0)`). Only when stdout *is* a tty does it take over the
screen (`AlternateScreen`/`HideCursor`/`MouseTerminal`/raw mode via
termion). `xless`'s `main.rs` must preserve this exactly (M0/M1 in the
[Roadmap](#roadmap)) — it's what makes both of the following work with
zero xless-specific vim configuration:

- **Suspend-and-run**, same as `:!less %` or `:!jless %` today:
  ```vim
  :!xless %
  ```
  Vim shells out, xless gets a real tty (vim's own), takes over the
  screen, and returns control to vim on `q`. This works today in plain
  vim, no plugin, no neovim required.

- **Embedded terminal split** (vim 8+ `:terminal`, or neovim):
  ```vim
  :vsplit
  :terminal xless %
  ```
  or in neovim, `:term xless %`. xless runs in a real pty inside the
  split, so it still gets a real tty and behaves identically — it just
  doesn't take over the whole vim session. This is the nicer day-to-day
  workflow and is what the shipped plugin (§3) wraps.

- **Pipe/filter mode**, for pulling canonicalized XML *into* a buffer
  rather than viewing it:
  ```vim
  :r !xless %
  ```
  or from a shell: `xless messy.xml | less`, `xless a.xml b.xml`. This
  only works because of the same isatty check — it's the same code path
  that makes `xless file.xml > pretty.xml` work as a formatter.

None of this requires `%` to be saved — `xless` reads the file from disk,
so unsaved buffer changes won't show up (see §3's temp-file handling for
that case).

### 2. The actual value-add: cursor-position-aware opening

Just shelling out to `xless %` loses your place — you're staring at
whatever XML file you were editing at line 340, and xless opens scrolled
to the top. The feature worth building (M4 in the [Roadmap](#roadmap)) is
passing the cursor's position through:

```vim
:execute '!xless --focus-line=' . line('.') . ' %'
```

This depends on the `--focus-line` machinery from [Architecture](#architecture)
§4 and §8: because every `Row.range` already points directly into the
*original* file's bytes (there's no separate re-rendered canonical buffer
for xless, unlike jless — see §4/§8.2), xless can take a source line
number, find the nearest/enclosing element via a straight binary search
over row start offsets, and:

1. Expand every collapsed ancestor of that element.
2. Scroll so that row is on-screen (ideally vertically centered, reusing
   `MoveFocusedLineToCenter`'s logic from the ported `viewer.rs`).
3. Focus it, so movement commands start from there.

This is the single feature that makes "view this in xless" meaningfully
better than "view this in a generic collapsible-tree XML viewer with no
idea what you were looking at."

### 3. Shipped plugin — implemented

Checked into this repo at `contrib/vim/` (not a separate plugin repo, so
version skew between xless and its vim glue isn't a problem), covering
both vim8 and neovim. See [`contrib/vim/README.md`](contrib/vim/README.md)
for install instructions (runtimepath, native packages, or a plugin
manager pointed at the `contrib/vim` subdirectory).

```
contrib/
  vim/
    plugin/xless.vim       " :Xless command, works in both vim8 and neovim
    ftplugin/xml.vim       " default <leader>xx mapping for filetype=xml
    README.md              " install instructions
```

Behavior (`xless#open()` in `plugin/xless.vim`):

- `:Xless` (no args) → current buffer's file, focused at the cursor line
  via `--focus-line` (§2 above — this is real now, not aspirational).
- `:Xless {file}` → explicit file, no focus line.
- If the buffer is modified (`&modified`), write to a temp file first
  (`tempname()`) and open *that*, so you're viewing your actual edits, not
  the stale on-disk version — mirrors how you'd want `:!jless %` to behave
  but doesn't require saving first. The temp file's line numbers still
  line up with the buffer's, so `--focus-line` still works correctly. A
  one-time `echom` names the temp path and warns that edits made inside
  that xless session won't flow back into the vim buffer — see §6 of
  [Editing model](#editing-model), which this resolves.
- Opens a `:terminal` split in neovim (`termopen`) or vim 8+
  (`:vertical terminal`) — not `:!`, so you don't lose vim's screen/redraw
  state (§1's embedded workflow) — falling back to suspend-and-run (`:!`)
  only on a vim old enough to lack `:terminal` at all.
- `ftplugin/xml.vim` adds a `<leader>xx` mapping (not `gx`/`gX`, which
  netrw and several XML/HTML ftplugins already claim) calling `:Xless`,
  disableable via `g:xless_no_default_mapping`.

### 4. Stretch: round-trip back to the editor

jless has a `:w` command (`Command::WriteFile` in `app.rs`) that writes
the current view (or a subtree) out to a file; xless's `:w` is a real save
of your edits (see [Editing model](#editing-model) §4), not just an
export, once M6 lands. The vim-integration analog worth considering once
`--focus-line` exists (M4): an `:edit` command inside xless itself that
shells out to `$EDITOR <original file> +<line>` for whatever row is
focused, using the focused row's `range.start` directly (no reverse
mapping needed — see §2 above). Combined with neovim's `:terminal`
supporting `:Xless` as a split, this gets you genuine two-way navigation:
jump from editor line → xless node, and from xless node → editor line,
without either side losing your place. **Not implemented** — §1, §2, and
§3 (all real now) already deliver "usable from vim"; this is still a
plausible future addition, not a gap anyone's blocked on.

### 5. The temp-file trick's editing guard — resolved

§3's "buffer is modified → open a temp copy" behavior was designed back
when xless was view-only, so there was no way to lose anything by viewing
a scratch copy. Now that xless can save (see [Editing model](#editing-model)),
§3's shipped plugin handles this: it warns (via `echom`) that the session
is viewing a scratch copy and that edits won't flow back into the vim
buffer, and relies on xless's own status bar already showing the real
path it's operating on (the temp file's) as the visible reminder, rather
than forcing the session read-only. See §6 of [Editing model](#editing-model)
for the same resolution from the editing side.

---

## Keybinding configuration

**Status: implemented in `src/config.rs`.** This section describes the
`~/.xless/settings.json` file that `:help`, `src/config.rs`'s generated
default file, and a couple of in-app messages all point back to.

The design goal (from the feature request that led to this file): let a
user override individual key-to-action bindings without forking the
binary or editing source, while making it hard to end up with a config
that "looks fine but doesn't do what you think" — every problem is
caught at load time, before the terminal ever goes into raw/alternate-
screen mode, with a specific and actionable error message. This mirrors
how a malformed input XML file is already handled (`xmlparser::parse`
errors print to stderr and exit 1) rather than introducing a second,
looser contract for a second kind of file.

### 1. File location and lifecycle

The file lives at `~/.xless/settings.json` (`config::default_config_path`)
— a dedicated directory under `$HOME`, not XDG's `~/.config/xless/`, by
explicit request. It's only consulted on the interactive path (`main.rs`,
after the `stdout_is_tty()` check): piping `xless file.xml | ...` or
redirecting to a file never reads or writes it, since there are no
keypresses to remap in that mode.

- **First run** (`~/.xless/` or `settings.json` doesn't exist yet):
  `config::load_or_init` creates the directory and writes out a default
  file — see §3 — then proceeds with the built-in default keymap. Nothing
  needs to be edited for xless to work; the file existing at all is just
  a starting point to edit *from*.
- **Every subsequent run**: the file is read, parsed, and validated (§4)
  from scratch. There's no daemon or file-watcher — "picked up" means "on
  the next launch," the same way editing `~/.vimrc` takes effect the next
  time you start vim, not while it's already running.
- **Deleting the file, or removing an entry from it**, falls back to that
  action's (or the whole keymap's) built-in default — nothing needs to
  stay in the file for xless to keep working.

### 2. Schema

```json
{
  "keys": {
    "move_down": ["j", "Down"],
    "quit": ["q"]
  },
  "yank_targets": {
    "pretty": "y"
  }
}
```

Two top-level sections, both optional (an empty `{}` — or a missing
section entirely — just means "use every default"):

- **`keys`**: action name → list of key specs. The list **replaces** that
  action's default list; it doesn't add to it. This was a deliberate
  choice over "append to defaults": knowing what an override *does*
  shouldn't require first memorizing what the defaults *were*. An empty
  list (`"quit": []`) is valid and means "no key triggers this" — `:`
  command mode is always reachable regardless, so nothing is ever
  permanently unreachable by emptying one action's key list.
- **`yank_targets`**: sub-key (pressed right after whatever triggers
  `yank_prefix`, default `y`) → single key spec, e.g. `yy`/`yl`/`yt`/
  `yn`/`yx` for pretty/one-line/text-content/tag-name/XPath. One key
  each, not a list, since there's exactly one way to reach each yank
  variant.

Run `xless` once with no config present (or delete
`~/.xless/settings.json` and re-run) to get a fully-populated example
with every current default filled in — this is the authoritative list of
valid action/target names and their current defaults; it's generated from
the same table (`config::ACTIONS`/`config::YANK_TARGETS`) that also drives
`:help`, so the two can never drift apart. A `"_readme"` key in the
generated file is a short reminder of this same syntax; it's ignored by
the parser (only `"keys"` and `"yank_targets"` are read) and safe to
delete or leave in place.

#### Key spec syntax

A key spec string is one of:

| Form | Meaning | Examples |
|---|---|---|
| A single character | That literal keypress | `"j"`, `"X"`, `"%"` |
| A named key | `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Backspace`, `Enter`, `Space`, `Tab` | `"Home"`, `"PageDown"` |
| `Ctrl-<char>` | That control combination | `"Ctrl-d"`, `"Ctrl-r"` |

Case matters for plain characters (`"g"` and `"G"` are different keys,
matching their different defaults below).

### 3. What you can't bind, and why

Three categories of key are rejected by validation even though nothing
else in the schema would flag them as malformed JSON:

- **`Esc` and `Ctrl-c`** — hardcoded, unconditional safety nets
  (`app.rs::handle_key` checks for them *before* the keymap is even
  consulted): `Esc` always cancels a pending count/prefix, `Ctrl-c`
  always force-quits. A broken or overly-creative config should never be
  able to lock someone out of quitting.
- **The digits `0`-`9`** — also consumed before the keymap lookup runs,
  by the count-prefix accumulator (`3j`, `42G`, vim-style). A config that
  bound an action to a digit would pass a naive validator, then silently
  never fire (the keypress is swallowed into the count buffer instead) —
  exactly the "looks fine, quietly doesn't work" failure this validation
  exists to catch. This was found via interactive testing of the config
  system itself, not something obvious from reading the schema.
- Binding the **same key to two different actions** (or two different
  yank targets) in the same file — ambiguous, so it's rejected rather
  than silently picking one. Note this is about the *final*, fully-
  resolved set of bindings: swapping two actions' keys in one file (e.g.
  `{"move_down": ["k"], "move_up": ["j"]}`) is fine — each action's
  default is cleared before any new bindings are checked for conflicts,
  so the intermediate state while processing the file never spuriously
  trips this check.

Everything else that can go wrong — invalid JSON, an unknown action or
yank-target name, an unparseable key spec — is also rejected with a
specific message naming the offending entry.

### 4. Failure behavior

`config::load_or_init` returns `Result<Keymap, String>`. `main.rs` treats
`Err` exactly like a malformed input file:

```
xless: /home/you/.xless/settings.json: action "quit": "9" can't be bound
to an action: digits are reserved for the count prefix (e.g. "3j", "42G")
and are always consumed before any keymap lookup, so binding "9" here
would silently never fire
```

printed to stderr, then `std::process::exit(1)` — **before** raw mode or
the alternate screen are entered, so the error is a normal, readable
terminal message, not something torn up by a half-initialized TUI. There
is no silent-fallback-to-defaults behavior: a broken config is treated as
a mistake worth surfacing, not something to paper over.

### 5. Design notes

- `config::ACTIONS` and `config::YANK_TARGETS` are the single source of
  truth for three things at once: the built-in default keymap, the JSON
  name ↔ action mapping used by the parser, and the live-generated
  `:help` text (`app.rs::build_help_text`). Before this file existed,
  `:help` was a hand-maintained static string with no relationship to the
  code it documented; it could (and did) drift. Driving all three off one
  table makes that class of drift structurally impossible rather than
  something to remember to keep in sync.
- Validation is two-phase (`config::build_keymap`): first every action
  name in the file is resolved and *all* of its default keys are cleared,
  then every new binding is inserted and checked for conflicts. Doing
  both steps together, one action at a time, would spuriously reject
  perfectly consistent configs like the `move_down`/`move_up` key-swap
  example in §3 — while inserting `move_down`'s new key, `move_up`'s
  *default* binding to that same key wouldn't have been cleared yet.
- The config file only affects **which key triggers which action** — the
  behavior each action performs is unchanged and unconfigurable. There's
  no way to define new actions or change what an existing one does from
  this file.

---

## Roadmap

### Status

M0–M4 and M6 are implemented (`src/`, `contrib/vim/`); M5's packaging
items are partly done (CI, no published-package step yet). What's real
today: mmap'd parsing with `u32`-compact rows; the interactive viewer
(navigation, collapse/expand, Line/Compact mode toggle, line numbers);
regex search (`/`, `?`, `n`, `N`); yank to the real system clipboard
(pretty subtree / one-line / text content / tag name / XPath) and paste
from it; `:` command mode (`:w`, `:wq`, `:q`, `:q!`, `:set number` /
`relativenumber`); mouse click-to-focus and wheel scroll; digit-count
movement prefixes; editing — rename, delete, insert-sibling,
text/attribute-blob edits, undo/redo, and `:w` save — with the
whole-document-reparse-on-commit approach the [Editing model](#editing-model)
section's status note describes (a documented simplification of that
section's original subtree-scoped design, not an oversight); and
configurable keybindings via `~/.xless/settings.json` (see
[Keybinding configuration](#keybinding-configuration)). The vim plugin
(`contrib/vim/`) ships `:Xless` and `--focus-line`. Not yet done: M3's
XPath-`@attr` addressing, M5's published packages, and the M2
search-strategy benchmarking §8.5 called for on real 100–200MB fixtures
(search works, just not benchmarked at that scale yet). See each module's
doc comment in `src/` for the specifics of what shipped vs. what a
milestone below still describes as forward-looking.

See [Architecture](#architecture) for the design this plan implements,
derived from reading jless (v0.9.0, ~11.3k LoC Rust), and extended in
Architecture §8–§9 to cover two requirements jless itself never had:
xless must (a) stay fast on 100–200MB XML files, and (b) work as a
standalone modal *editor*, not just a read-only pager. See
[Vim integration](#vim-integration) for how this is meant to be driven
from vim/neovim — a convenience layer on top of the standalone tool, not
a dependency of it — and [Editing model](#editing-model) for the editing
design referenced in M6 below.

### Guiding constraints

1. jless already proved the flatten-tree-into-rows TUI architecture works;
   the risk in *this* project isn't "will that pattern work," it's "does
   the XML data model ([Architecture](#architecture) §3) hold up," and
   separately, "does the mmap'd-original / patch-based design (§8, §9)
   actually hit the 100–200MB performance target once real fixtures exist
   to measure." Sequencing below front-loads the parser + data model + a
   bare-bones renderer — including the large-file-specific choices from
   day one, since retrofitting mmap/compact-rows/two-phase-parsing after
   building on jless's simpler in-memory approach would mean redoing the
   data model layer, not extending it.
2. Get a read-only viewer solid (M0–M5) *before* editing (M6). Editing's
   patch/splice/undo model ([Editing model](#editing-model) §3) leans
   directly on the traversal and lazy-reparse machinery the viewer needs
   anyway — building it on top of a proven viewer is lower-risk than
   building both at once.

### Milestones

#### M0 — Skeleton, data model, and large-file validation (no TUI yet)
- `Cargo.toml`: `quick-xml`, `memmap2`, `clap` (derive), `unicode-width`,
  `unicode-segmentation`, `regex`, `lazy_static`. (Deferred: `termion`,
  `rustyline`, `signal-hook`, `libc`, `isatty`, `clipboard` — not needed
  until M2.)
- `flatxml.rs`: `Value`/`Row`/`FlatXml` per [Architecture](#architecture)
  §3, using the compact `u32`-based `Row` from §8.3 from the start, with
  the traversal methods ported from jless's `flatjson.rs` (§2/§6):
  `next_visible_row`, `prev_visible_row`, `next_item`, `prev_item`,
  `expand`/`collapse`/`toggle_collapsed`, `first_visible_ancestor`.
- `xmlparser.rs`: `quick-xml`-based parser over an `memmap2`-mapped file,
  producing `FlatXml` whose `Row.range`s point directly into the mapped
  original bytes (§4/§8.2 — no second canonical-text buffer). Implement
  §8.4's phase-1 structural pass; phase-2 lazy attribute/whitespace detail
  can be a simple non-memoized "compute on read" stub for now — the
  memoization/background-thread fallback only matters once it's measured
  (M0 exit criteria below), not before.
- CLI (`main.rs` minimal): read file/stdin, parse, print a pretty-printed
  rendering to stdout, exit. This alone is a useful `xmlfmt`-equivalent
  and the thing [Vim integration](#vim-integration)'s pipe-mode recipes
  depend on.
- Port jless's `flatjson.rs` unit test *style* (the `assert_visited_rows`/
  `assert_visited_items`/collapse tests) onto small hand-written XML
  fixtures — these tests are cheap insurance that the traversal port is
  behaviorally correct before any rendering exists to eyeball.
- Generate (or source) 2–3 synthetic XML fixtures in the 100–200MB range
  with varied shape (deeply nested, wide/flat with many siblings,
  attribute-heavy, mixed-content-heavy) and benchmark phase-1 parse time
  and peak memory against [Architecture](#architecture) §8.6's targets.
- **Exit criteria**: `xless < some.xml` prints readable, correctly nested
  XML; traversal unit tests pass; **the 100–200MB fixtures parse within
  §8.6's budget** — if they don't, that's this milestone's real finding,
  and it should feed back into §8.4's background-thread fallback or a
  revised budget before M1 starts, not get silently carried forward.

#### M1 — Minimal interactive viewer (Compact mode only)
- Port `terminal.rs`, `types.rs`, `truncatedstrview.rs`, `input.rs`
  essentially unchanged ([Architecture](#architecture) §6 "keep" row).
- `xmlviewer.rs` (renamed from jless's `viewer.rs`): `Action` enum +
  `perform_action`, ported with `FlatJson`→`FlatXml`.
- `lineprinter.rs`: Compact-mode rendering only (defer Line mode to M3).
- `highlighting.rs`: minimal palette (tag/attr/text/comment).
- `app.rs`: event loop, only the movement keys (`j k h l w b g G ^ $ Home
  End % space c C e E`), no search/yank/command-mode yet.
- `main.rs`: wire up raw-mode/alternate-screen termion setup exactly as
  jless's `main.rs` does, gated on `isatty::stdout_isatty()` so pipe mode
  (M0) keeps working.
- **Exit criteria**: can open a real-world XML file (pick something with
  mixed content and a few dozen elements, e.g. an RSS feed or a Maven
  `pom.xml`), navigate and collapse/expand the whole tree, quit cleanly.

#### M2 — Search, yank, command mode, mouse
- Port `search.rs`'s wrap-around/expand-collapsed-containers logic; decide
  and implement the actual search strategy per [Architecture](#architecture)
  §8.5 (regex-over-mmap'd-bytes vs. row-kind-restricted search) using the
  M0 large fixtures to benchmark both before picking.
- `y`/`p` submenus per [Architecture](#architecture) §7.3 (pretty subtree,
  one-line subtree, text content, tag name, XPath, attribute value).
- `:` command mode via rustyline: `:w`, `:set number`/`relativenumber`,
  help.
- Mouse click-to-focus, wheel scroll.
- Clipboard integration (`clipboard` crate, same as jless — note its
  Linux X11 dev-package dependency, carry the same README caveat jless
  has).
- **Exit criteria**: feature parity with jless's interaction surface,
  modulo the format-specific yank/path targets.

#### M3 — Line mode + XPath path building
- Line mode per [Architecture](#architecture) §5 table.
- `build_path_to_node` → XPath generator, resolving the same-tag-sibling
  1-based indexing question flagged in [Architecture](#architecture) §7.1.
- Decide and (if warranted) implement attributes-as-rows in Line mode
  ([Architecture](#architecture) §7.2) based on how painful attribute-heavy
  real files feel in M2 testing.

#### M4 — Vim integration
- Everything in [Vim integration](#vim-integration). Concretely:
  `--focus-line`/`--focus-pos` CLI flag, resolved directly from
  `Row.range.start` (§4/§8.2 — no separate field needed now, since ranges
  already point at original-file bytes), and the shipped vim/neovim
  plugin.
- `:edit`-style command inside xless that shells out to `$EDITOR
  <file> +<line>` using the reverse mapping (focused row → its
  `range.start`), for round-tripping back to the editor.

#### M5 — Polish & packaging (read-only viewer, feature-complete)
- Config file — **implemented**, see [Keybinding configuration](#keybinding-configuration)
  (jless has none today — worth a fresh look, since a hardcoded-color
  16-color palette per `terminal.rs` is the one place jless punts on user
  customization).
- `cargo install xless`, README with install instructions matching
  jless's package-manager table (start with source install + cargo, add
  package manager entries opportunistically).
- CI (matching jless's `ci.yml` shape: fmt/clippy/test on macOS+Linux),
  including the M0 large-fixture benchmark as a tracked (not necessarily
  blocking) CI metric so performance regressions are visible.
- Checkpoint: this is a legitimate v1 release point on its own — a fast
  read-only jless-for-XML with vim integration — if editing (M6) needs
  more design time than expected.

#### M6 — Editing
- Full design: [Editing model](#editing-model). Implementation order
  within the milestone:
  1. Patch overlay + splice-rebuild-subtree mechanics (§2–§3 of that
     section), since search/lazy-detail-parsing (M0/M2) already need
     "reparse just this subtree" — editing extends that machinery rather
     than inventing new machinery.
  2. Streaming `:w` save (§4 of that section) — validate against an M0
     large fixture with a small edit applied, confirm save time/memory
     tracks "stream the file through," not "file size again in RAM."
  3. Normal-mode structural commands + Insert-mode field editing (§1, §5
     of that section).
  4. Undo/redo.
  5. Resolve §6 of that section (temp-file-from-vim guard) before this
     ships alongside [Vim integration](#vim-integration)'s existing
     temp-file recipe.
- Confirm [Architecture](#architecture) §7.4's scope question (constrained
  structural edits vs. free-text-everywhere) before starting — it changes
  §1's command surface materially.

### Explicitly out of scope for v1

- XSD/DTD validation.
- XSLT.
- Namespace-URI-aware querying (namespaces are opaque text — see
  [Architecture](#architecture) §4).
- Lenient/tag-soup HTML parsing (well-formed XML only; revisit only if
  there's real demand).
- Windows support (jless notes this as "planned" and still hasn't shipped
  it as of the version studied; same story here — termion is Unix-only,
  a `crossterm` swap would be needed, deliberately deferred).

### Testing strategy

- Unit tests on `flatxml.rs` traversal (M0), mirroring jless's
  `flatjson.rs` test module almost mechanically.
- Golden-file tests: canonical pretty-print of curated fixture files
  (self-closing tags, mixed content, CDATA, comments, PIs, doctype,
  deeply nested, empty elements, attribute-heavy elements, namespaced
  tags) checked into `tests/fixtures/`.
- `lineprinter.rs` rendering tests using `indoc` (jless does this too, per
  its `dev-dependencies`), asserting exact rendered lines for both modes.
- No end-to-end TUI/input-loop tests planned (jless doesn't have these
  either — `input.rs`/`app.rs` are exercised manually); keep parity here
  rather than inventing test infra jless itself decided wasn't worth it.
- Large-fixture benchmarks (M0, tracked in CI from M5) for parse time,
  memory, and — once M2/M6 exist — search and save time, against
  [Architecture](#architecture) §8.6's targets.
- M6: patch/splice/undo correctness tests (apply edit → verify resulting
  row tree matches parsing the edited text directly from scratch; undo →
  verify byte-identical to pre-edit original) and a save round-trip test
  on a large fixture (small edit, `:w`, verify unchanged regions are
  byte-identical to the original file).
