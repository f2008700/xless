# xless implementation plan

## Status

M0–M4 and M6 are implemented (`src/`, `contrib/vim/`); M5's packaging
items are partly done (CI, no published-package step yet). What's real
today: mmap'd parsing with `u32`-compact rows; the interactive viewer
(navigation, collapse/expand, Line/Compact mode toggle, line numbers);
regex search (`/`, `?`, `n`, `N`); yank to the real system clipboard
(pretty subtree / one-line / text content / tag name / XPath) and paste
from it; `:` command mode (`:w`, `:wq`, `:q`, `:q!`, `:set number` /
`relativenumber`); mouse click-to-focus and wheel scroll; digit-count
movement prefixes; and editing — rename, delete, insert-sibling,
text/attribute-blob edits, undo/redo, and `:w` save — with the
whole-document-reparse-on-commit approach `EDITING.md`'s status note
describes (a documented simplification of that doc's original
subtree-scoped design, not an oversight). The vim plugin
(`contrib/vim/`) ships `:Xless` and `--focus-line`. Not yet done: M3's
XPath-`@attr` addressing, M5's config file and published packages, and
the M2 search-strategy benchmarking §8.5 called for on real 100–200MB
fixtures (search works, just not benchmarked at that scale yet). See
each module's doc comment in `src/` for the specifics of what shipped
vs. what a milestone below still describes as forward-looking.

See `ARCHITECTURE.md` for the design this plan implements, derived from
reading `~/git/jless` (v0.9.0, ~11.3k LoC Rust), and extended in
ARCHITECTURE.md §8–§9 to cover two requirements jless itself never had:
xless must (a) stay fast on 100–200MB XML files, and (b) work as a
standalone modal *editor*, not just a read-only pager. See
`VIM_INTEGRATION.md` for how this is meant to be driven from vim/neovim —
a convenience layer on top of the standalone tool, not a dependency of it
— and `EDITING.md` for the editing design referenced in M6 below.

## Guiding constraints

1. jless already proved the flatten-tree-into-rows TUI architecture works;
   the risk in *this* project isn't "will that pattern work," it's "does
   the XML data model (ARCHITECTURE.md §3) hold up," and separately,
   "does the mmap'd-original / patch-based design (§8, §9) actually hit
   the 100–200MB performance target once real fixtures exist to measure."
   Sequencing below front-loads the parser + data model + a bare-bones
   renderer — including the large-file-specific choices from day one,
   since retrofitting mmap/compact-rows/two-phase-parsing after building
   on jless's simpler in-memory approach would mean redoing the data
   model layer, not extending it.
2. Get a read-only viewer solid (M0–M5) *before* editing (M6). Editing's
   patch/splice/undo model (`EDITING.md` §3) leans directly on the
   traversal and lazy-reparse machinery the viewer needs anyway — building
   it on top of a proven viewer is lower-risk than building both at once.

## Milestones

### M0 — Skeleton, data model, and large-file validation (no TUI yet)
- `Cargo.toml`: `quick-xml`, `memmap2`, `clap` (derive), `unicode-width`,
  `unicode-segmentation`, `regex`, `lazy_static`. (Deferred: `termion`,
  `rustyline`, `signal-hook`, `libc`, `isatty`, `clipboard` — not needed
  until M2.)
- `flatxml.rs`: `Value`/`Row`/`FlatXml` per ARCHITECTURE.md §3, using the
  compact `u32`-based `Row` from §8.3 from the start, with the traversal
  methods ported from jless's `flatjson.rs` (§2/§6): `next_visible_row`,
  `prev_visible_row`, `next_item`, `prev_item`,
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
  and the thing `VIM_INTEGRATION.md`'s pipe-mode recipes depend on.
- Port jless's `flatjson.rs` unit test *style* (the `assert_visited_rows`/
  `assert_visited_items`/collapse tests) onto small hand-written XML
  fixtures — these tests are cheap insurance that the traversal port is
  behaviorally correct before any rendering exists to eyeball.
- Generate (or source) 2–3 synthetic XML fixtures in the 100–200MB range
  with varied shape (deeply nested, wide/flat with many siblings,
  attribute-heavy, mixed-content-heavy) and benchmark phase-1 parse time
  and peak memory against ARCHITECTURE.md §8.6's targets.
- **Exit criteria**: `xless < some.xml` prints readable, correctly nested
  XML; traversal unit tests pass; **the 100–200MB fixtures parse within
  §8.6's budget** — if they don't, that's this milestone's real finding,
  and it should feed back into §8.4's background-thread fallback or a
  revised budget before M1 starts, not get silently carried forward.

### M1 — Minimal interactive viewer (Compact mode only)
- Port `terminal.rs`, `types.rs`, `truncatedstrview.rs`, `input.rs`
  essentially unchanged (ARCHITECTURE.md §6 "keep" row).
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

### M2 — Search, yank, command mode, mouse
- Port `search.rs`'s wrap-around/expand-collapsed-containers logic; decide
  and implement the actual search strategy per ARCHITECTURE.md §8.5
  (regex-over-mmap'd-bytes vs. row-kind-restricted search) using the M0
  large fixtures to benchmark both before picking.
- `y`/`p` submenus per ARCHITECTURE.md §7.3 (pretty subtree, one-line
  subtree, text content, tag name, XPath, attribute value).
- `:` command mode via rustyline: `:w`, `:set number`/`relativenumber`,
  help.
- Mouse click-to-focus, wheel scroll.
- Clipboard integration (`clipboard` crate, same as jless — note its
  Linux X11 dev-package dependency, carry the same README caveat jless
  has).
- **Exit criteria**: feature parity with jless's interaction surface,
  modulo the format-specific yank/path targets.

### M3 — Line mode + XPath path building
- Line mode per ARCHITECTURE.md §5 table.
- `build_path_to_node` → XPath generator, resolving the same-tag-sibling
  1-based indexing question flagged in ARCHITECTURE.md §7.1.
- Decide and (if warranted) implement attributes-as-rows in Line mode
  (ARCHITECTURE.md §7.2) based on how painful attribute-heavy real files
  feel in M2 testing.

### M4 — Vim integration
- Everything in `VIM_INTEGRATION.md`. Concretely: `--focus-line`/
  `--focus-pos` CLI flag, resolved directly from `Row.range.start`
  (§4/§8.2 — no separate field needed now, since ranges already point at
  original-file bytes), and the shipped vim/neovim plugin.
- `:edit`-style command inside xless that shells out to `$EDITOR
  <file> +<line>` using the reverse mapping (focused row → its
  `range.start`), for round-tripping back to the editor.

### M5 — Polish & packaging (read-only viewer, feature-complete)
- Config file (jless has none today — worth a fresh look, since a
  hardcoded-color 16-color palette per `terminal.rs` is the one place
  jless punts on user customization).
- `cargo install xless`, README with install instructions matching
  jless's package-manager table (start with source install + cargo, add
  package manager entries opportunistically).
- CI (matching jless's `ci.yml` shape: fmt/clippy/test on macOS+Linux),
  including the M0 large-fixture benchmark as a tracked (not necessarily
  blocking) CI metric so performance regressions are visible.
- Checkpoint: this is a legitimate v1 release point on its own — a fast
  read-only jless-for-XML with vim integration — if editing (M6) needs
  more design time than expected.

### M6 — Editing
- Full design: `EDITING.md`. Implementation order within the milestone:
  1. Patch overlay + splice-rebuild-subtree mechanics (`EDITING.md` §2–§3),
     since search/lazy-detail-parsing (M0/M2) already need "reparse just
     this subtree" — editing extends that machinery rather than inventing
     new machinery.
  2. Streaming `:w` save (`EDITING.md` §4) — validate against an M0 large
     fixture with a small edit applied, confirm save time/memory tracks
     "stream the file through," not "file size again in RAM."
  3. Normal-mode structural commands + Insert-mode field editing
     (`EDITING.md` §1, §5).
  4. Undo/redo.
  5. Resolve `EDITING.md` §6 (temp-file-from-vim guard) before this ships
     alongside `VIM_INTEGRATION.md`'s existing temp-file recipe.
- Confirm ARCHITECTURE.md §7.4's scope question (constrained structural
  edits vs. free-text-everywhere) before starting — it changes §1's
  command surface materially.

## Explicitly out of scope for v1

- XSD/DTD validation.
- XSLT.
- Namespace-URI-aware querying (namespaces are opaque text — see
  ARCHITECTURE.md §4).
- Lenient/tag-soup HTML parsing (well-formed XML only; revisit only if
  there's real demand).
- Windows support (jless notes this as "planned" and still hasn't shipped
  it as of the version studied; same story here — termion is Unix-only,
  a `crossterm` swap would be needed, deliberately deferred).

## Testing strategy

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
  ARCHITECTURE.md §8.6's targets.
- M6: patch/splice/undo correctness tests (apply edit → verify resulting
  row tree matches parsing the edited text directly from scratch; undo →
  verify byte-identical to pre-edit original) and a save round-trip test
  on a large fixture (small edit, `:w`, verify unchanged regions are
  byte-identical to the original file).
