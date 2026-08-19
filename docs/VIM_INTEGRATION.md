# Using xless from vim

**xless is standalone-first** (ARCHITECTURE.md §9): `xless somefile.xml`
on its own, with no vim involved at all, is the whole product — a full
interactive session with vim-style navigation *and* editing
(`EDITING.md`). Nothing here is a prerequisite for using xless. This doc
covers driving it *from* vim specifically, because that's an explicit,
important target workflow, not because xless needs vim to be useful. It's
split into what works "for free" because of decisions already made in
ARCHITECTURE.md, and what needs a small shipped plugin.

## 1. Works for free, if we keep jless's terminal contract

jless behaves exactly like `less`: it detects whether stdout is a real
terminal, and if not, it just prints and exits (`main.rs`:
`isatty::stdout_isatty()` check → `print_pretty_printed_input` then
`std::process::exit(0)`). Only when stdout *is* a tty does it take over the
screen (`AlternateScreen`/`HideCursor`/`MouseTerminal`/raw mode via
termion). `xless`'s `main.rs` must preserve this exactly (M0/M1 in
PLAN.md) — it's what makes both of the following work with zero
xless-specific vim configuration:

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

## 2. The actual value-add: cursor-position-aware opening

Just shelling out to `xless %` loses your place — you're staring at
whatever XML file you were editing at line 340, and xless opens scrolled
to the top. The feature worth building (PLAN.md M4) is passing the
cursor's position through:

```vim
:execute '!xless --focus-line=' . line('.') . ' %'
```

This depends on the `--focus-line` machinery from ARCHITECTURE.md §4 and
§8: because every `Row.range` already points directly into the *original*
file's bytes (there's no separate re-rendered canonical buffer for xless,
unlike jless — see §4/§8.2), xless can take a source line number, find
the nearest/enclosing element via a straight binary search over row
start offsets, and:

1. Expand every collapsed ancestor of that element.
2. Scroll so that row is on-screen (ideally vertically centered, reusing
   `MoveFocusedLineToCenter`'s logic from the ported `viewer.rs`).
3. Focus it, so movement commands start from there.

This is the single feature that makes "view this in xless" meaningfully
better than "view this in a generic collapsible-tree XML viewer with no
idea what you were looking at."

## 3. Shipped plugin — implemented

Checked into this repo at `contrib/vim/` (not a separate plugin repo, so
version skew between xless and its vim glue isn't a problem), covering
both vim8 and neovim. See `contrib/vim/README.md` for install
instructions (runtimepath, native packages, or a plugin manager pointed
at the `contrib/vim` subdirectory).

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
  that xless session won't flow back into the vim buffer — see
  `EDITING.md` §6, which this resolves.
- Opens a `:terminal` split in neovim (`termopen`) or vim 8+
  (`:vertical terminal`) — not `:!`, so you don't lose vim's screen/redraw
  state (§1's embedded workflow) — falling back to suspend-and-run (`:!`)
  only on a vim old enough to lack `:terminal` at all.
- `ftplugin/xml.vim` adds a `<leader>xx` mapping (not `gx`/`gX`, which
  netrw and several XML/HTML ftplugins already claim) calling `:Xless`,
  disableable via `g:xless_no_default_mapping`.

## 4. Stretch: round-trip back to the editor

jless has a `:w` command (`Command::WriteFile` in `app.rs`) that writes
the current view (or a subtree) out to a file; xless's `:w` is a real save
of your edits (`EDITING.md` §4), not just an export, once M6 lands. The
vim-integration analog worth considering once `--focus-line` exists (M4):
an `:edit` command inside xless itself that shells out to
`$EDITOR <original file> +<line>` for whatever row is focused, using the
focused row's `range.start` directly (no reverse mapping needed — see §2
above). Combined with neovim's `:terminal` supporting `:Xless` as a split,
this gets you genuine two-way navigation: jump from editor line → xless
node, and from xless node → editor line, without either side losing your
place. **Not implemented** — §1, §2, and §3 (all real now) already
deliver "usable from vim"; this is still a plausible future addition, not
a gap anyone's blocked on.

## 5. The temp-file trick's editing guard — resolved

§3's "buffer is modified → open a temp copy" behavior was designed back
when xless was view-only, so there was no way to lose anything by viewing
a scratch copy. Now that xless can save (`EDITING.md`), §3's shipped
plugin handles this: it warns (via `echom`) that the session is viewing a
scratch copy and that edits won't flow back into the vim buffer, and
relies on xless's own status bar already showing the real path it's
operating on (the temp file's) as the visible reminder, rather than
forcing the session read-only. See `EDITING.md` §6 for the same
resolution from the editing side.
