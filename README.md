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
`:` command mode, mouse support, and editing (rename/delete/insert/edit
content, undo/redo, save) with a validate-and-rollback safety net against
ever saving invalid XML. See `docs/PLAN.md`'s status section for the
precise cut line. Not yet packaged for distribution (no published crate
or platform packages yet — build from source with `cargo build --release`).

## Install (from source)

```sh
git clone <this repo> && cd xless
cargo install --path .
```

## Docs

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the design (derived
from studying [`jless`](https://jless.io)) and its large-file performance
strategy, [`docs/EDITING.md`](docs/EDITING.md) for the editing model,
[`docs/PLAN.md`](docs/PLAN.md) for the milestone roadmap and current
status, and [`docs/VIM_INTEGRATION.md`](docs/VIM_INTEGRATION.md) plus
[`contrib/vim/README.md`](contrib/vim/README.md) for the vim/neovim
plugin.