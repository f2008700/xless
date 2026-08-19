# xless.vim

The vim/neovim plugin described in [`docs/VIM_INTEGRATION.md`](../../docs/VIM_INTEGRATION.md).
Provides `:Xless [file]`, which opens the current (or a given) file in
`xless` in a terminal split, focused at your cursor's line.

## Install

`xless` itself must be on your `$PATH` first (`cargo install --path .`
from the repo root, or wherever you've built/installed it).

**Plain vim / neovim, manual:** add this directory to your runtimepath:

```vim
set runtimepath+=~/path/to/xless/contrib/vim
```

**vim 8+ native packages:**

```sh
mkdir -p ~/.vim/pack/xless/start
ln -s /path/to/xless/contrib/vim ~/.vim/pack/xless/start/xless
```

**neovim:**

```sh
mkdir -p ~/.local/share/nvim/site/pack/xless/start
ln -s /path/to/xless/contrib/vim ~/.local/share/nvim/site/pack/xless/start/xless
```

**Plugin managers** (vim-plug, packer.nvim, lazy.nvim, ...): point them at
this `contrib/vim` subdirectory rather than the repo root, e.g. with
vim-plug:

```vim
Plug '~/path/to/xless', { 'rtp': 'contrib/vim' }
```

## Use

- `:Xless` — open the current buffer's file, focused at the cursor line.
- `:Xless path/to/other.xml` — open an explicit file.
- `<leader>xx` in an `xml`-filetype buffer — same as `:Xless`.
- `g:xless_command` — override if `xless` isn't on `$PATH` under that
  name (default: `'xless'`).
- `g:xless_no_default_mapping` — set to disable the `<leader>xx` mapping
  if it conflicts with something else in your config.
