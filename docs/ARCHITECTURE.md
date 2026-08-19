# xless architecture

`xless` is `jless` for XML: a terminal pager that shows a syntax-highlighted,
collapsible tree view of an XML document, with vim-style movement and
regex search. This document records what was learned from reading the
`jless` source (`~/git/jless`, ~11.3k lines of Rust) and how those ideas
translate — or don't — to XML.

## 1. What jless actually is, structurally

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

## 2. Navigation model (kept as-is)

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

## 3. The XML data model (`flatxml.rs`, new)

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

## 4. Parsing (`xmlparser.rs`, new)

Use [`quick-xml`](https://docs.rs/quick-xml) as the tokenizer, in pull-parser
(`Reader::read_event`) mode, analogous to how `jsonparser.rs` drives the
`logos`-generated `JsonToken` lexer by hand. `quick-xml` is well-maintained,
has no required allocation-heavy DOM step, supports namespaces as an
opt-in, and — importantly — gives byte offsets into the *original* input
for every event (`Reader::buffer_position`).

That last point turns out to matter twice over: it's both how xless avoids
the second-full-copy memory cost flagged in §1 for large files, and how it
gets a feature jless didn't need — **mapping an editor's cursor line in
the *original* file to a row in the viewer** (see `VIM_INTEGRATION.md`)
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
  (`EDITING.md`) — "unchanged" literally means "no patch covers this
  range of the original file," which is exactly the representation a
  streaming save wants anyway.

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
explore a lenient "HTML-soup" mode; explicitly out of scope for v1 (§ in
PLAN.md).

## 5. Two viewing modes (kept as a concept, renamed) — implemented

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

## 6. File-by-file port plan

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

## 7. Open design questions to resolve early (not blocking a v1 skeleton)

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
   regions, per `EDITING.md` §1. Attribute values are edited as one raw
   blob per element, not individually addressable fields — a further
   simplification from even the "constrained" plan, since attributes
   still aren't separate rows (point 2 above remains open/unimplemented).

## 8. Large-file performance strategy (100–200MB inputs)

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
   the default representation from M0 (PLAN.md), not a later retrofit.

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
   sub-second. PLAN.md's M0 exit criteria should include measuring a real
   100–200MB fixture against these numbers, not just checking output
   correctness on small files.

## 9. Standalone-first, and editing (not just viewing)

Two scope decisions, best-effort interpretation of stated requirements —
flagged for confirmation rather than treated as fully settled:

**Standalone-first.** `xless <file>` must be the *whole product* on its
own — a full interactive, vim-keybinding-driven session — with zero
dependency on vim. This was already implicit in following jless's
`main.rs` contract (§6, `VIM_INTEGRATION.md` §1: `xless file.xml` grabs
its own tty and runs, full stop; being launchable *from* vim via `:!`/
`:terminal` is just a consequence of behaving like a normal terminal
program, not a separate integration path). Restated explicitly here so
it's an intentional property of `main.rs`/§4/§8, not an accident of
following jless too closely: vim integration (`VIM_INTEGRATION.md`) is a
convenience layer on top of a standalone tool, never a prerequisite for
using it.

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
later." Full design: `EDITING.md`.
