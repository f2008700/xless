// Adapted from jless's src/options.rs. No --json/--yaml-style format
// flags (xless only reads XML, per docs/ARCHITECTURE.md §4). The
// --line-numbers/--relative-line-numbers pair mirrors jless's own
// slightly unusual clap setup for getting both `--foo`/`--no-foo` to work
// with sensible defaults — see jless's options.rs for the original
// explanation of why it's structured this way.

use std::path::PathBuf;

use clap::{ArgAction, Parser, ValueEnum};

use crate::viewer::Mode;

/// xless — a command-line XML viewer and editor. Pipes like `less`: run
/// `xless file.xml` in a terminal for the interactive viewer, or
/// `xless file.xml | ...` / `xless file.xml > out.xml` to just
/// pretty-print and exit.
#[derive(Debug, Parser)]
#[command(name = "xless", version)]
pub struct Opt {
    /// Input file. Reads from stdin if omitted or '-' is given.
    pub input: Option<PathBuf>,

    /// Initial viewing mode. In line mode, closing tags and genuinely
    /// empty elements are always shown as their own rows. In compact
    /// mode (the default), closing tags are elided and empty elements
    /// are shown as self-closing `<tag/>`. Toggle with `m`.
    #[arg(short, long, value_enum, hide_possible_values = true, default_value_t = CliMode::Compact)]
    pub mode: CliMode,

    /// Don't show line numbers.
    #[arg(short = 'N', long = "no-line-numbers", action = ArgAction::SetFalse)]
    pub show_line_numbers: bool,

    /// Show line numbers (default).
    #[arg(
        short = 'n',
        long = "line-numbers",
        overrides_with = "show_line_numbers"
    )]
    pub _show_line_numbers_hidden: bool,

    /// Show the line number relative to the currently focused row.
    #[arg(
        short = 'r',
        long = "relative-line-numbers",
        overrides_with = "_show_relative_line_numbers_hidden"
    )]
    pub show_relative_line_numbers: bool,

    /// Don't show relative line numbers (default).
    #[arg(short = 'R', long = "no-relative-line-numbers")]
    _show_relative_line_numbers_hidden: bool,

    /// Number of lines to keep as padding between the focused row and the
    /// top/bottom of the screen.
    #[arg(long = "scrolloff", default_value_t = 3)]
    pub scrolloff: u16,

    /// Focus the element containing this 1-based line number of the
    /// *original source file* on startup, expanding collapsed ancestors
    /// as needed. See docs/VIM_INTEGRATION.md.
    #[arg(long = "focus-line")]
    pub focus_line: Option<usize>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum CliMode {
    Line,
    Compact,
}

impl From<CliMode> for Mode {
    fn from(m: CliMode) -> Mode {
        match m {
            CliMode::Line => Mode::Line,
            CliMode::Compact => Mode::Compact,
        }
    }
}
