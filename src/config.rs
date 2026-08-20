// User-configurable keybindings, loaded from ~/.xless/settings.json. This
// is the piece that turns app.rs's key dispatch from "a hardcoded match
// statement" into "a hardcoded match statement over *actions*, with a
// data-driven layer in front deciding which physical key triggers which
// action" — the behavior each action performs stays fixed in app.rs (that
// part was never what was asked for), only the key-to-action mapping is
// configurable.
//
// Design choices, and why:
//
// - File format is a flat `{"keys": {"action_name": ["key", ...]}}` map,
//   not a list of raw key-sequences, so validation can check each entry
//   independently (unknown action name / unparseable key spec / key
//   claimed by two actions) and report exactly what's wrong, rather than
//   accepting a keybinding DSL that could be malformed in more subtle,
//   harder-to-validate ways. "So that a user doesn't end up writing
//   random stuff" — this is the mechanism for that: `load_or_init`
//   returns `Err(String)` with a specific, actionable message on any
//   problem, and main.rs treats that exactly like a malformed input XML
//   file — print the error, exit before ever touching the terminal,
//   rather than silently falling back to defaults and leaving the user
//   wondering why their remap didn't take effect.
// - An action's list of keys in the file *replaces* its default list,
//   rather than adding to it — simpler mental model ("this is now the
//   complete set of keys for this action") than having to know the
//   defaults to figure out what an override does. An empty list is valid
//   and means "no key triggers this" (`:` command mode remains reachable
//   either way).
// - `Esc` and `Ctrl-c` can never be bound to an action (checked in
//   `build_keymap`, independent of what the user writes) — they're
//   handled before the keymap lookup even runs (app.rs), as an
//   unconditional safety net so a broken or overly-creative config can
//   never lock someone out of quitting or clearing a pending count.
// - `ACTIONS`/`YANK_TARGETS` below are the single source of truth for
//   three things at once: the default keymap, the JSON name<->action
//   mapping, and the live-generated `:help` text (app.rs's
//   `build_help_text`) — so `:help` always reflects whatever's actually
//   configured, not a hardcoded copy that can drift from it (which is
//   exactly what happened before this module existed: `:help` was a
//   static string that had no relationship to the code it was
//   documenting).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use termion::event::Key;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AppAction {
    MoveDown,
    MoveUp,
    MoveLeft,
    MoveRight,
    FocusParent,
    FocusNextSibling,
    FocusPrevSibling,
    FocusFirstSibling,
    FocusLastSibling,
    FocusTop,
    FocusBottom,
    FocusMatchingPair,
    PageDown,
    PageUp,
    ScrollDown,
    ScrollUp,
    MoveFocusedLineToTop,
    MoveFocusedLineToCenter,
    MoveFocusedLineToBottom,
    ToggleCollapsed,
    CollapseNodeAndSiblings,
    ExpandNodeAndSiblings,
    ToggleMode,
    TogglePathStyle,
    SearchForward,
    SearchBackward,
    SearchNext,
    SearchPrev,
    YankPrefix,
    PasteAfter,
    PasteBefore,
    EditContent,
    Rename,
    InsertAfter,
    InsertBefore,
    DeletePrefix,
    Undo,
    Redo,
    CommandMode,
    Quit,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum YankTarget {
    Pretty,
    OneLine,
    TextContent,
    TagName,
    XPath,
}

pub struct ActionInfo {
    pub action: AppAction,
    pub name: &'static str,
    pub default_keys: &'static [Key],
    pub section: &'static str,
    pub description: &'static str,
}

pub struct YankTargetInfo {
    pub target: YankTarget,
    pub name: &'static str,
    pub default_key: Key,
    pub description: &'static str,
}

macro_rules! keys {
    ($($k:expr),* $(,)?) => { &[$($k),*] };
}

pub const ACTIONS: &[ActionInfo] = &[
    ActionInfo {
        action: AppAction::MoveDown,
        name: "move_down",
        default_keys: keys![Key::Char('j'), Key::Down],
        section: "Movement",
        description: "move down",
    },
    ActionInfo {
        action: AppAction::MoveUp,
        name: "move_up",
        default_keys: keys![Key::Char('k'), Key::Up],
        section: "Movement",
        description: "move up",
    },
    ActionInfo {
        action: AppAction::MoveLeft,
        name: "move_left",
        default_keys: keys![Key::Char('h'), Key::Left],
        section: "Movement",
        description: "collapse / focus parent",
    },
    ActionInfo {
        action: AppAction::MoveRight,
        name: "move_right",
        default_keys: keys![Key::Char('l'), Key::Right],
        section: "Movement",
        description: "expand / focus first child",
    },
    ActionInfo {
        action: AppAction::FocusParent,
        name: "focus_parent",
        default_keys: keys![Key::Char('H')],
        section: "Movement",
        description: "focus parent",
    },
    ActionInfo {
        action: AppAction::FocusNextSibling,
        name: "focus_next_sibling",
        default_keys: keys![Key::Char('J')],
        section: "Movement",
        description: "focus next sibling",
    },
    ActionInfo {
        action: AppAction::FocusPrevSibling,
        name: "focus_prev_sibling",
        default_keys: keys![Key::Char('K')],
        section: "Movement",
        description: "focus previous sibling",
    },
    ActionInfo {
        action: AppAction::FocusFirstSibling,
        name: "focus_first_sibling",
        default_keys: keys![Key::Char('^')],
        section: "Movement",
        description: "focus first sibling",
    },
    ActionInfo {
        action: AppAction::FocusLastSibling,
        name: "focus_last_sibling",
        default_keys: keys![Key::Char('$')],
        section: "Movement",
        description: "focus last sibling",
    },
    ActionInfo {
        action: AppAction::FocusTop,
        name: "focus_top",
        default_keys: keys![Key::Char('g'), Key::Home],
        section: "Movement",
        description: "focus top of document",
    },
    ActionInfo {
        action: AppAction::FocusBottom,
        name: "focus_bottom",
        default_keys: keys![Key::Char('G'), Key::End],
        section: "Movement",
        description: "focus bottom (or row N with a count prefix, e.g. 42G)",
    },
    ActionInfo {
        action: AppAction::FocusMatchingPair,
        name: "focus_matching_pair",
        default_keys: keys![Key::Char('%')],
        section: "Movement",
        description: "jump to matching open/close tag",
    },
    ActionInfo {
        action: AppAction::PageDown,
        name: "page_down",
        default_keys: keys![Key::Ctrl('d'), Key::PageDown],
        section: "Scrolling",
        description: "page down",
    },
    ActionInfo {
        action: AppAction::PageUp,
        name: "page_up",
        default_keys: keys![Key::Ctrl('u'), Key::PageUp],
        section: "Scrolling",
        description: "page up",
    },
    ActionInfo {
        action: AppAction::ScrollDown,
        name: "scroll_down",
        default_keys: keys![Key::Ctrl('e')],
        section: "Scrolling",
        description: "scroll down one line",
    },
    ActionInfo {
        action: AppAction::ScrollUp,
        name: "scroll_up",
        default_keys: keys![Key::Ctrl('y')],
        section: "Scrolling",
        description: "scroll up one line",
    },
    ActionInfo {
        action: AppAction::MoveFocusedLineToTop,
        name: "move_focused_line_to_top",
        default_keys: keys![Key::Char('t')],
        section: "Scrolling",
        description: "move focused line to top of screen",
    },
    ActionInfo {
        action: AppAction::MoveFocusedLineToCenter,
        name: "move_focused_line_to_center",
        default_keys: keys![Key::Char('z')],
        section: "Scrolling",
        description: "move focused line to center of screen",
    },
    ActionInfo {
        action: AppAction::MoveFocusedLineToBottom,
        name: "move_focused_line_to_bottom",
        default_keys: keys![Key::Char('b')],
        section: "Scrolling",
        description: "move focused line to bottom of screen",
    },
    ActionInfo {
        action: AppAction::ToggleCollapsed,
        name: "toggle_collapsed",
        default_keys: keys![Key::Char(' '), Key::Char('\n')],
        section: "Viewing",
        description: "toggle collapse of the focused element",
    },
    ActionInfo {
        action: AppAction::CollapseNodeAndSiblings,
        name: "collapse_node_and_siblings",
        default_keys: keys![Key::Char('c')],
        section: "Viewing",
        description: "collapse the focused element and its siblings",
    },
    ActionInfo {
        action: AppAction::ExpandNodeAndSiblings,
        name: "expand_node_and_siblings",
        default_keys: keys![Key::Char('e')],
        section: "Viewing",
        description: "expand the focused element and its siblings",
    },
    ActionInfo {
        action: AppAction::ToggleMode,
        name: "toggle_mode",
        default_keys: keys![Key::Char('m')],
        section: "Viewing",
        description: "toggle Line / Compact mode",
    },
    ActionInfo {
        action: AppAction::TogglePathStyle,
        name: "toggle_path_style",
        default_keys: keys![Key::Char('X')],
        section: "Viewing",
        description: "toggle the header between XPath and a literal breadcrumb path",
    },
    ActionInfo {
        action: AppAction::SearchForward,
        name: "search_forward",
        default_keys: keys![Key::Char('/')],
        section: "Search",
        description: "search forward (regex)",
    },
    ActionInfo {
        action: AppAction::SearchBackward,
        name: "search_backward",
        default_keys: keys![Key::Char('?')],
        section: "Search",
        description: "search backward (regex)",
    },
    ActionInfo {
        action: AppAction::SearchNext,
        name: "search_next",
        default_keys: keys![Key::Char('n')],
        section: "Search",
        description: "repeat search, same direction",
    },
    ActionInfo {
        action: AppAction::SearchPrev,
        name: "search_prev",
        default_keys: keys![Key::Char('N')],
        section: "Search",
        description: "repeat search, opposite direction",
    },
    ActionInfo {
        action: AppAction::YankPrefix,
        name: "yank_prefix",
        default_keys: keys![Key::Char('y')],
        section: "Yank / paste",
        description: "start a yank (followed by a yank-target key — see below)",
    },
    ActionInfo {
        action: AppAction::PasteAfter,
        name: "paste_after",
        default_keys: keys![Key::Char('p')],
        section: "Yank / paste",
        description: "paste clipboard as next sibling",
    },
    ActionInfo {
        action: AppAction::PasteBefore,
        name: "paste_before",
        default_keys: keys![Key::Char('P')],
        section: "Yank / paste",
        description: "paste clipboard as previous sibling",
    },
    ActionInfo {
        action: AppAction::EditContent,
        name: "edit_content",
        default_keys: keys![Key::Char('i')],
        section: "Editing",
        description: "edit text / attributes of the focused row",
    },
    ActionInfo {
        action: AppAction::Rename,
        name: "rename",
        default_keys: keys![Key::Char('r')],
        section: "Editing",
        description: "rename the focused element's tag",
    },
    ActionInfo {
        action: AppAction::InsertAfter,
        name: "insert_after",
        default_keys: keys![Key::Char('o')],
        section: "Editing",
        description: "insert a new element as next sibling",
    },
    ActionInfo {
        action: AppAction::InsertBefore,
        name: "insert_before",
        default_keys: keys![Key::Char('O')],
        section: "Editing",
        description: "insert a new element as previous sibling",
    },
    ActionInfo {
        action: AppAction::DeletePrefix,
        name: "delete_node",
        default_keys: keys![Key::Char('d')],
        section: "Editing",
        description: "delete the focused element (press twice, like vim's dd)",
    },
    ActionInfo {
        action: AppAction::Undo,
        name: "undo",
        default_keys: keys![Key::Char('u')],
        section: "Editing",
        description: "undo",
    },
    ActionInfo {
        action: AppAction::Redo,
        name: "redo",
        default_keys: keys![Key::Ctrl('r')],
        section: "Editing",
        description: "redo",
    },
    ActionInfo {
        action: AppAction::CommandMode,
        name: "command_mode",
        default_keys: keys![Key::Char(':')],
        section: "Other",
        description: "enter command mode (:w, :q, :help, ...)",
    },
    ActionInfo {
        action: AppAction::Quit,
        name: "quit",
        default_keys: keys![Key::Char('q')],
        section: "Other",
        description: "quit (warns first if there are unsaved edits)",
    },
];

pub const YANK_TARGETS: &[YankTargetInfo] = &[
    YankTargetInfo {
        target: YankTarget::Pretty,
        name: "pretty",
        default_key: Key::Char('y'),
        description: "pretty-printed subtree",
    },
    YankTargetInfo {
        target: YankTarget::OneLine,
        name: "one_line",
        default_key: Key::Char('l'),
        description: "subtree as one line",
    },
    YankTargetInfo {
        target: YankTarget::TextContent,
        name: "text_content",
        default_key: Key::Char('t'),
        description: "concatenated text content",
    },
    YankTargetInfo {
        target: YankTarget::TagName,
        name: "tag_name",
        default_key: Key::Char('n'),
        description: "tag name",
    },
    YankTargetInfo {
        target: YankTarget::XPath,
        name: "xpath",
        default_key: Key::Char('x'),
        description: "XPath to focused node",
    },
];

impl AppAction {
    fn info(self) -> &'static ActionInfo {
        ACTIONS
            .iter()
            .find(|a| a.action == self)
            .expect("every AppAction has an ACTIONS entry")
    }

    pub fn name(self) -> &'static str {
        self.info().name
    }

    fn from_name(name: &str) -> Option<AppAction> {
        ACTIONS.iter().find(|a| a.name == name).map(|a| a.action)
    }
}

impl YankTarget {
    fn info(self) -> &'static YankTargetInfo {
        YANK_TARGETS
            .iter()
            .find(|t| t.target == self)
            .expect("every YankTarget has a YANK_TARGETS entry")
    }

    pub fn name(self) -> &'static str {
        self.info().name
    }

    fn from_name(name: &str) -> Option<YankTarget> {
        YANK_TARGETS
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.target)
    }
}

#[derive(Debug)]
pub struct Keymap {
    pub actions: HashMap<Key, AppAction>,
    pub yank_targets: HashMap<Key, YankTarget>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut actions = HashMap::new();
        for info in ACTIONS {
            for &key in info.default_keys {
                actions.insert(key, info.action);
            }
        }
        let mut yank_targets = HashMap::new();
        for info in YANK_TARGETS {
            yank_targets.insert(info.default_key, info.target);
        }
        Keymap {
            actions,
            yank_targets,
        }
    }
}

impl Keymap {
    /// All keys currently bound to `action`, in a stable (sorted-by-
    /// display-string) order — used by app.rs's `:help` generation.
    pub fn keys_for(&self, action: AppAction) -> Vec<Key> {
        let mut keys: Vec<Key> = self
            .actions
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(k, _)| *k)
            .collect();
        keys.sort_by_key(key_spec_to_string);
        keys
    }

    pub fn key_for_yank_target(&self, target: YankTarget) -> Option<Key> {
        self.yank_targets
            .iter()
            .find(|(_, t)| **t == target)
            .map(|(k, _)| *k)
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    keys: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    yank_targets: BTreeMap<String, String>,
}

/// Parses a human-written key spec ("j", "Ctrl-d", "Down", "Space", ...)
/// into the `termion::event::Key` it represents. The inverse of
/// `key_spec_to_string` below (round-trips for every key `ACTIONS`/
/// `YANK_TARGETS` actually uses as a default, though not necessarily for
/// every theoretically constructible `Key`).
fn parse_key_spec(raw: &str) -> Result<Key, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty key spec".to_string());
    }

    if let Some(rest) = s.strip_prefix("Ctrl-").or_else(|| s.strip_prefix("ctrl-")) {
        let mut chars = rest.chars();
        let c = chars
            .next()
            .ok_or_else(|| format!("invalid key spec {raw:?}: nothing after \"Ctrl-\""))?;
        if chars.next().is_some() {
            return Err(format!(
                "invalid key spec {raw:?}: \"Ctrl-\" combos take exactly one character"
            ));
        }
        return Ok(Key::Ctrl(c.to_ascii_lowercase()));
    }

    match s {
        "Up" => return Ok(Key::Up),
        "Down" => return Ok(Key::Down),
        "Left" => return Ok(Key::Left),
        "Right" => return Ok(Key::Right),
        "Home" => return Ok(Key::Home),
        "End" => return Ok(Key::End),
        "PageUp" => return Ok(Key::PageUp),
        "PageDown" => return Ok(Key::PageDown),
        "Backspace" => return Ok(Key::Backspace),
        "Enter" => return Ok(Key::Char('\n')),
        "Space" => return Ok(Key::Char(' ')),
        "Tab" => return Ok(Key::Char('\t')),
        "Esc" | "Escape" => {
            return Err(
                "\"Esc\" is reserved (it always cancels a pending count/prefix) and can't be bound to an action".to_string(),
            )
        }
        _ => {}
    }

    let mut chars = s.chars();
    let c = chars.next().expect("checked non-empty above");
    if chars.next().is_some() {
        return Err(format!(
            "invalid key spec {raw:?}: expected a single character, a named key \
             (Up/Down/Left/Right/Home/End/PageUp/PageDown/Backspace/Enter/Space/Tab), \
             or Ctrl-<char>"
        ));
    }
    Ok(Key::Char(c))
}

/// The inverse of `parse_key_spec` — used both to generate the default
/// config file's text and by `Keymap::keys_for` to sort/display keys
/// consistently.
pub fn key_spec_to_string(key: &Key) -> String {
    match key {
        Key::Up => "Up".to_string(),
        Key::Down => "Down".to_string(),
        Key::Left => "Left".to_string(),
        Key::Right => "Right".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::PageUp => "PageUp".to_string(),
        Key::PageDown => "PageDown".to_string(),
        Key::Backspace => "Backspace".to_string(),
        Key::Char('\n') => "Enter".to_string(),
        Key::Char(' ') => "Space".to_string(),
        Key::Char('\t') => "Tab".to_string(),
        Key::Char(c) => c.to_string(),
        Key::Ctrl(c) => format!("Ctrl-{c}"),
        other => format!("{other:?}"),
    }
}

fn check_not_reserved(key: Key, key_str: &str) -> Result<(), String> {
    if key == Key::Esc || key == Key::Ctrl('c') {
        return Err(format!(
            "\"{key_str}\" is reserved (Esc and Ctrl-c always cancel/force-quit) and can't be bound to an action"
        ));
    }
    // Digits are unconditionally consumed by app.rs's count-prefix buffer
    // (`3j`, `42G`, ...) *before* the keymap is ever consulted — see
    // handle_key's digit branch, which runs ahead of both the prefix-key
    // check and the keymap lookup. A config binding an action or yank
    // target to a digit would therefore pass validation but silently
    // never fire (the keypress is swallowed into the count buffer
    // instead), which is exactly the class of "looks fine, quietly
    // doesn't work" mistake this validation exists to catch rather than
    // let through. Found via live pty verification of the config system,
    // not user-reported.
    if let Key::Char(c @ '0'..='9') = key {
        return Err(format!(
            "\"{key_str}\" can't be bound to an action: digits are reserved for the count prefix \
             (e.g. \"3j\", \"42G\") and are always consumed before any keymap lookup, so binding \
             \"{c}\" here would silently never fire"
        ));
    }
    Ok(())
}

fn build_keymap(raw: RawConfig) -> Result<Keymap, String> {
    let mut keymap = Keymap::default();

    // Two phases, deliberately not "resolve and insert one action at a
    // time": resolve every action name first and clear *all* of their
    // default bindings, THEN insert every override's new keys. Doing it
    // in one pass per action would spuriously reject a perfectly
    // consistent config — e.g. swapping j/k by writing
    // `{"move_down": ["k"], "move_up": ["j"]}`: while inserting
    // move_down's new key "k", move_up hasn't been processed yet, so its
    // *default* "k" binding would still look like a conflict, even though
    // the swap is fine once both overrides are applied. Clearing all
    // overridden actions' defaults up front means only *genuine*
    // conflicts (two overrides both wanting the same key for different
    // actions) surface as errors. (Found by a test written for the
    // "replace, don't add" behavior above, which — written slightly
    // differently — would have hit exactly this false-positive.)
    let mut resolved_actions = Vec::with_capacity(raw.keys.len());
    for (name, key_strs) in &raw.keys {
        let action = AppAction::from_name(name).ok_or_else(|| {
            format!(
                "unknown action \"{name}\" under \"keys\" — see the generated default file \
                 or docs/CONFIG.md for valid action names"
            )
        })?;
        keymap.actions.retain(|_, a| *a != action);
        resolved_actions.push((action, name, key_strs));
    }
    for (action, name, key_strs) in resolved_actions {
        for key_str in key_strs {
            let key = parse_key_spec(key_str).map_err(|e| format!("action \"{name}\": {e}"))?;
            check_not_reserved(key, key_str).map_err(|e| format!("action \"{name}\": {e}"))?;
            if let Some(&existing) = keymap.actions.get(&key) {
                if existing != action {
                    return Err(format!(
                        "key \"{key_str}\" is bound to both \"{}\" and \"{name}\" — \
                         each key can only trigger one action",
                        existing.name()
                    ));
                }
            }
            keymap.actions.insert(key, action);
        }
    }

    // Same two-phase treatment, same reasoning, for the (much smaller)
    // yank-target map.
    let mut resolved_targets = Vec::with_capacity(raw.yank_targets.len());
    for (name, key_str) in &raw.yank_targets {
        let target = YankTarget::from_name(name)
            .ok_or_else(|| format!("unknown yank target \"{name}\" under \"yank_targets\""))?;
        keymap.yank_targets.retain(|_, t| *t != target);
        resolved_targets.push((target, name, key_str));
    }
    for (target, name, key_str) in resolved_targets {
        let key = parse_key_spec(key_str).map_err(|e| format!("yank target \"{name}\": {e}"))?;
        check_not_reserved(key, key_str).map_err(|e| format!("yank target \"{name}\": {e}"))?;
        if let Some(&existing) = keymap.yank_targets.get(&key) {
            if existing != target {
                return Err(format!(
                    "yank-target key \"{key_str}\" is bound to both \"{}\" and \"{name}\"",
                    existing.name()
                ));
            }
        }
        keymap.yank_targets.insert(key, target);
    }

    Ok(keymap)
}

/// `~/.xless/settings.json` — chosen over an XDG-style
/// `~/.config/xless/` path because that's literally what was asked for;
/// `$HOME` (not a `dirs`-crate lookup) since xless already only targets
/// Unix (ARCHITECTURE.md's termion dependency rules out Windows anyway).
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".xless").join("settings.json"))
}

/// Loads `path`, validating it strictly (see the module doc comment for
/// why a validation failure is a hard error, not a silent fallback to
/// defaults). If `path` doesn't exist yet, writes out a fresh default
/// config there (containing every current default binding, so it's both
/// a working config and a self-documenting starting point to edit) and
/// returns the default keymap.
pub fn load_or_init(path: &Path) -> Result<Keymap, String> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        std::fs::write(path, generate_default_config_text())
            .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        return Ok(Keymap::default());
    }

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let raw: RawConfig = serde_json::from_str(&text)
        .map_err(|e| format!("{}: invalid JSON: {e}", path.display()))?;
    build_keymap(raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn generate_default_config_text() -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str(&format!(
        "  \"_readme\": {},\n",
        serde_json::to_string(
            "xless keybinding overrides. Each action's key list REPLACES its default \
             (it doesn't add to it) -- an empty list means no key triggers it. Key syntax: \
             a single character (\"j\"), a named key (Up/Down/Left/Right/Home/End/PageUp/ \
             PageDown/Backspace/Enter/Space/Tab), or Ctrl-<char> (\"Ctrl-d\"). Esc, Ctrl-c, and \
             the digits 0-9 (reserved for the count prefix, e.g. \"3j\") can't be rebound. \
             Delete this file, or any entry in it, to fall back to the default. See docs/CONFIG.md."
        )
        .unwrap()
    ));
    out.push_str("  \"keys\": {\n");
    for (i, info) in ACTIONS.iter().enumerate() {
        let keys_json: Vec<String> = info
            .default_keys
            .iter()
            .map(|k| serde_json::to_string(&key_spec_to_string(k)).unwrap())
            .collect();
        let comma = if i + 1 == ACTIONS.len() { "" } else { "," };
        out.push_str(&format!(
            "    \"{}\": [{}]{comma}\n",
            info.name,
            keys_json.join(", ")
        ));
    }
    out.push_str("  },\n");
    out.push_str("  \"yank_targets\": {\n");
    for (i, info) in YANK_TARGETS.iter().enumerate() {
        let comma = if i + 1 == YANK_TARGETS.len() { "" } else { "," };
        out.push_str(&format!(
            "    \"{}\": {}{comma}\n",
            info.name,
            serde_json::to_string(&key_spec_to_string(&info.default_key)).unwrap()
        ));
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Keymap, String> {
        let raw: RawConfig = serde_json::from_str(json).map_err(|e| e.to_string())?;
        build_keymap(raw)
    }

    #[test]
    fn test_default_keymap_matches_actions_table() {
        let km = Keymap::default();
        assert_eq!(km.actions.get(&Key::Char('j')), Some(&AppAction::MoveDown));
        assert_eq!(km.actions.get(&Key::Char('q')), Some(&AppAction::Quit));
        assert_eq!(
            km.yank_targets.get(&Key::Char('x')),
            Some(&YankTarget::XPath)
        );
    }

    #[test]
    fn test_override_replaces_not_adds() {
        // "A" isn't any action's default key, so this exercises "replace"
        // semantics without also tripping the (correct, separately
        // tested below) same-key-two-actions conflict check — remapping
        // to a key another *untouched* action still defaults to (e.g.
        // "k", which is move_up's default) is expected to fail, since
        // that really would be ambiguous; see
        // test_conflicting_binding_is_rejected.
        let km = parse(r#"{"keys": {"move_down": ["A"]}}"#).unwrap();
        // "A" now moves down...
        assert_eq!(km.actions.get(&Key::Char('A')), Some(&AppAction::MoveDown));
        // ...and neither "j" nor "Down" do anything anymore (replaced,
        // not appended to).
        assert_eq!(km.actions.get(&Key::Char('j')), None);
        assert_eq!(km.actions.get(&Key::Down), None);
        // Untouched actions keep their defaults.
        assert_eq!(km.actions.get(&Key::Char('q')), Some(&AppAction::Quit));
    }

    #[test]
    fn test_unknown_action_name_is_rejected() {
        let err = parse(r#"{"keys": {"mvoe_dwon": ["j"]}}"#).unwrap_err();
        assert!(err.contains("unknown action"), "unexpected error: {err}");
        assert!(err.contains("mvoe_dwon"), "unexpected error: {err}");
    }

    #[test]
    fn test_unparseable_key_is_rejected() {
        let err = parse(r#"{"keys": {"move_down": ["jk"]}}"#).unwrap_err();
        assert!(err.contains("invalid key spec"), "unexpected error: {err}");
    }

    #[test]
    fn test_swapping_two_actions_keys_together_works() {
        // The natural way to swap j/k: remap both in the same file. Each
        // individual remap collides with the *other's default* in
        // isolation, but resolved together there's no actual ambiguity —
        // build_keymap must not reject this.
        let km = parse(r#"{"keys": {"move_down": ["k"], "move_up": ["j"]}}"#).unwrap();
        assert_eq!(km.actions.get(&Key::Char('k')), Some(&AppAction::MoveDown));
        assert_eq!(km.actions.get(&Key::Char('j')), Some(&AppAction::MoveUp));
    }

    #[test]
    fn test_conflicting_binding_is_rejected() {
        let err = parse(r#"{"keys": {"move_down": ["z"], "move_up": ["z"]}}"#).unwrap_err();
        assert!(err.contains("bound to both"), "unexpected error: {err}");
    }

    #[test]
    fn test_esc_and_ctrl_c_cannot_be_bound() {
        assert!(parse(r#"{"keys": {"quit": ["Esc"]}}"#).is_err());
        assert!(parse(r#"{"keys": {"quit": ["Ctrl-c"]}}"#).is_err());
    }

    #[test]
    fn test_digits_cannot_be_bound() {
        // Digits are unconditionally swallowed by app.rs's count-prefix
        // buffer before the keymap is ever consulted (see
        // check_not_reserved's doc comment) — binding one to an action
        // must be rejected at load time rather than silently never firing.
        let err = parse(r#"{"keys": {"quit": ["5"]}}"#).unwrap_err();
        assert!(err.contains("count prefix"), "unexpected error: {err}");
        // '0' is included too, even though app.rs treats a lone leading
        // '0' as the focus_first_sibling-adjacent "^"-style command
        // rather than the start of a count — it's still consumed by the
        // same digit branch ahead of the keymap lookup, so it's just as
        // dead a binding.
        assert!(parse(r#"{"keys": {"quit": ["0"]}}"#).is_err());
        assert!(
            parse(r#"{"yank_targets": {"pretty": "7"}}"#).is_err(),
            "digits must be rejected for yank targets too"
        );
    }

    #[test]
    fn test_empty_key_list_unbinds_action() {
        let km = parse(r#"{"keys": {"quit": []}}"#).unwrap();
        assert_eq!(km.actions.get(&Key::Char('q')), None);
        assert!(km.keys_for(AppAction::Quit).is_empty());
    }

    #[test]
    fn test_invalid_json_is_rejected() {
        assert!(parse("{not json").is_err());
    }

    #[test]
    fn test_yank_target_override() {
        let km = parse(r#"{"yank_targets": {"xpath": "p"}}"#).unwrap();
        assert_eq!(
            km.yank_targets.get(&Key::Char('p')),
            Some(&YankTarget::XPath)
        );
        assert_eq!(km.yank_targets.get(&Key::Char('x')), None);
    }

    #[test]
    fn test_unknown_yank_target_name_is_rejected() {
        let err = parse(r#"{"yank_targets": {"xpaht": "x"}}"#).unwrap_err();
        assert!(
            err.contains("unknown yank target"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_key_spec_round_trip_for_all_defaults() {
        for info in ACTIONS {
            for &k in info.default_keys {
                let s = key_spec_to_string(&k);
                assert_eq!(
                    parse_key_spec(&s).unwrap(),
                    k,
                    "round-trip failed for action {} key {:?} -> {s:?}",
                    info.name,
                    k
                );
            }
        }
        for info in YANK_TARGETS {
            let s = key_spec_to_string(&info.default_key);
            assert_eq!(parse_key_spec(&s).unwrap(), info.default_key);
        }
    }

    #[test]
    fn test_generated_default_config_is_valid_and_round_trips() {
        let text = generate_default_config_text();
        let km = parse(&text).unwrap();
        let default = Keymap::default();
        // Generating the default file and re-parsing it should produce
        // exactly the same keymap as never having a file at all.
        assert_eq!(km.actions.len(), default.actions.len());
        for (k, a) in &default.actions {
            assert_eq!(km.actions.get(k), Some(a));
        }
    }
}
