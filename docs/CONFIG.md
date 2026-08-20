# Keybinding configuration

**Status: implemented in `src/config.rs`.** This doc describes the
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

## 1. File location and lifecycle

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

## 2. Schema

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

### Key spec syntax

A key spec string is one of:

| Form | Meaning | Examples |
|---|---|---|
| A single character | That literal keypress | `"j"`, `"X"`, `"%"` |
| A named key | `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Backspace`, `Enter`, `Space`, `Tab` | `"Home"`, `"PageDown"` |
| `Ctrl-<char>` | That control combination | `"Ctrl-d"`, `"Ctrl-r"` |

Case matters for plain characters (`"g"` and `"G"` are different keys,
matching their different defaults below).

## 3. What you can't bind, and why

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

## 4. Failure behavior

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

## 5. Design notes

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
