// Entry point. Mirrors jless's src/main.rs contract deliberately (see
// README.md's Vim integration section §1): if stdout isn't a real terminal, just
// pretty-print the parsed document and exit — this is what makes
// `xless file.xml | ...`, `xless file.xml > out.xml`, and vim's
// `:r !xless %` all work with no special-casing. Only when stdout *is* a
// tty do we take over the screen (raw mode + alternate screen) and run
// the interactive viewer.

mod app;
mod clipboard;
mod config;
mod document;
mod edit;
mod flatxml;
mod highlighting;
mod input;
mod lineprinter;
mod options;
mod path;
mod screenwriter;
mod search;
mod terminal;
mod types;
mod viewer;
mod xmlparser;

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;

use clap::Parser;
use termion::cursor::HideCursor;
use termion::input::MouseTerminal;
use termion::raw::IntoRawMode;
use termion::screen::IntoAlternateScreen;

use app::App;
use document::{Document, Source};
use options::Opt;
use types::TTYDimensions;
use viewer::Viewer;

fn main() {
    let opt = Opt::parse();

    let (source, filename, file_path) = match load_source(&opt) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("xless: unable to read input: {err}");
            std::process::exit(1);
        }
    };

    let flat = match xmlparser::parse(&source) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("xless: {err}");
            std::process::exit(1);
        }
    };

    let doc = Document { source, flat };

    if !stdout_is_tty() {
        print!("{}", lineprinter::pretty_printed(&doc));
        return;
    }

    // Loaded (and, on first run, written out) only for the interactive
    // path — a keybinding typo shouldn't block `xless file.xml | ...`
    // pipe usage, which never reads a keymap at all. Validated strictly
    // and *before* the terminal goes into raw/alternate-screen mode
    // (config::load_or_init never touches the terminal itself), so a bad
    // config prints a normal, readable error and exits — the same
    // contract malformed input XML already gets, rather than a cryptic
    // half-drawn TUI or a silent fallback that leaves a remap looking
    // like it just didn't work.
    let keymap = match config::default_config_path() {
        Some(path) => match config::load_or_init(&path) {
            Ok(km) => km,
            Err(err) => {
                eprintln!("xless: {err}");
                std::process::exit(1);
            }
        },
        None => config::Keymap::default(),
    };

    let dimensions = app::query_terminal_size().unwrap_or(TTYDimensions {
        width: 80,
        height: 24,
    });

    let mut viewer = Viewer::new(doc, dimensions);
    viewer.scrolloff_setting = opt.scrolloff;
    viewer.mode = opt.mode.into();
    // Status bar (1 row) + the path header screenwriter.rs always draws
    // (screenwriter::HEADER_ROWS) — fixed for the process's lifetime, so
    // (unlike `dimensions` itself) this never needs to be touched again
    // on a resize. See `Viewer::reserved_rows`'s doc comment for why
    // viewer.rs's scrolling math needs to know this at all.
    viewer.reserved_rows = 1 + screenwriter::HEADER_ROWS;

    let mut focus_line_warning = None;
    if let Some(line) = opt.focus_line {
        match byte_offset_of_line(&viewer.doc.source, line) {
            Some(offset) => viewer.focus_source_offset(offset),
            None => {
                focus_line_warning = Some(format!(
                    "--focus-line={line}: file only has {} line(s); staying at the top",
                    count_lines(&viewer.doc.source)
                ));
            }
        }
    }

    let raw = match io::stdout().into_raw_mode() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("xless: unable to set raw terminal mode: {err}");
            std::process::exit(1);
        }
    };
    let alt = match raw.into_alternate_screen() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("xless: unable to switch to alternate screen: {err}");
            std::process::exit(1);
        }
    };
    let hidden = HideCursor::from(alt);
    let mouse = MouseTerminal::from(hidden);

    let mut app = App::new(viewer, filename, file_path, keymap, mouse);
    app.screen_writer.show_line_numbers = opt.show_line_numbers;
    app.screen_writer.show_relative_line_numbers = opt.show_relative_line_numbers;
    if let Some(msg) = focus_line_warning {
        app.set_initial_message(msg);
    }
    app.run(input::get_input());
}

fn stdout_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) != 0 }
}

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 }
}

fn load_source(opt: &Opt) -> io::Result<(Source, String, Option<PathBuf>)> {
    let read_stdin = || -> io::Result<(Source, String, Option<PathBuf>)> {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf)?;
        Ok((Source::Owned(buf), "STDIN".to_string(), None))
    };

    match &opt.input {
        None => {
            if stdin_is_tty() {
                eprintln!("xless: missing filename (\"xless --help\" for help)");
                std::process::exit(1);
            }
            read_stdin()
        }
        Some(path) if path == &PathBuf::from("-") => read_stdin(),
        Some(path) => {
            let file = File::open(path)?;
            // SAFETY: the standard caveat of mmap applies — the file could
            // be modified or truncated by another process while mapped.
            // xless is a viewer over a file the user pointed it at, the
            // same trust model tools like ripgrep operate under; not
            // guarding against concurrent external mutation in v1.
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            let filename = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            Ok((Source::Mapped(mmap), filename, Some(path.clone())))
        }
    }
}

/// Converts a 1-based line number in the original source into a byte
/// offset, for `--focus-line` (README.md's Vim integration section §2). Linear scan —
/// fine for a one-shot startup lookup even on a large file; this is not
/// on any interactive hot path.
fn byte_offset_of_line(source: &[u8], line: usize) -> Option<u32> {
    if line <= 1 {
        return Some(0);
    }
    let mut seen = 1usize;
    for (i, b) in source.iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == line {
                return Some((i + 1) as u32);
            }
        }
    }
    None
}

/// Total line count, for the `--focus-line` out-of-range warning message.
fn count_lines(source: &[u8]) -> usize {
    let newlines = source.iter().filter(|&&b| b == b'\n').count();
    // A final line with no trailing newline still counts as a line.
    if source.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}
