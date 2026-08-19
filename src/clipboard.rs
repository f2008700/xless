// Yank-to-system-clipboard. jless links against the `clipboard` crate,
// which pulls in X11 dev headers on Linux (its own README calls this
// out). Rather than take that native-dependency cost, xless shells out to
// whichever platform clipboard tool is on $PATH — `pbcopy` (macOS),
// `wl-copy` (Wayland), `xclip`/`xsel` (X11) — same approach many
// terminal-only tools use. If none are found, yank still "succeeds" in
// the sense that the app doesn't crash; the status bar just says so
// instead of silently pretending it worked.

use std::io::Write;
use std::process::{Command, Stdio};

fn candidates() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ]
}

/// Tries each known clipboard tool in turn; returns `Ok(())` on the first
/// one that accepts the text, or `Err` naming the problem if none work.
pub fn copy(text: &str) -> Result<(), String> {
    for (cmd, args) in candidates() {
        let child = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(_) => continue, // not installed / not on PATH — try the next one
        };

        let write_result = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(text.as_bytes());

        // Always reap the child, even if the write failed (e.g. a broken
        // pipe because the tool exited early) — `&&`'s short-circuiting
        // used to skip `child.wait()` entirely in that case, leaking a
        // zombie process for the life of the xless session (found by
        // review, confirmed independently twice).
        let wait_result = child.wait();

        if write_result.is_ok() && wait_result.map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
    }

    Err("no clipboard tool found (tried pbcopy/wl-copy/xclip/xsel)".to_string())
}

fn paste_candidates() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("pbpaste", &[]),
        ("wl-paste", &[]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ]
}

/// The inverse of `copy` — reads the system clipboard's current text, for
/// `p`/`P` paste (see app.rs). Backs "paste a node you just yanked (or
/// copied from anywhere else) as a new sibling."
pub fn paste() -> Result<String, String> {
    for (cmd, args) in paste_candidates() {
        let output = Command::new(cmd).args(*args).stderr(Stdio::null()).output();
        if let Ok(output) = output {
            if output.status.success() {
                return String::from_utf8(output.stdout)
                    .map_err(|_| "clipboard contents were not valid UTF-8".to_string());
            }
        }
    }
    Err("no clipboard tool found (tried pbpaste/wl-paste/xclip/xsel)".to_string())
}
