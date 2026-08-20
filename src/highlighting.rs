// Color palette for XML syntax elements. Structurally analogous to
// jless's src/highlighting.rs, but the palette itself is new (tag/
// attribute/text/comment/CData/PI/doctype instead of JSON's key/string/
// number/boolean/null) — see README.md's Architecture section §6.
//
// v1 simplification vs. jless: one color per *row kind*, applied to the
// whole rendered line, rather than jless's character-level highlighting
// (separate colors for keys vs. string values vs. punctuation within a
// single line). Full token-level highlighting (e.g. attribute names vs.
// attribute values vs. the tag name itself, all different colors on one
// line) is a reasonable fast-follow, not required to have a usable
// syntax-highlighted viewer.

use crate::flatxml::Value;
use crate::terminal::{self, Color, Style};

pub const TAG: Color = terminal::LIGHT_BLUE;
// ATTR_NAME/ATTR_VALUE/PUNCTUATION aren't used by `style_for` below yet —
// see the module doc comment: whole-line coloring today, reserved for
// the character-level highlighting fast-follow (would color an
// attribute's name/value/quotes differently from its tag on the same
// line).
#[allow(dead_code)]
pub const ATTR_NAME: Color = terminal::YELLOW;
#[allow(dead_code)]
pub const ATTR_VALUE: Color = terminal::LIGHT_GREEN;
pub const TEXT: Color = terminal::WHITE;
pub const COMMENT: Color = terminal::LIGHT_BLACK;
pub const CDATA: Color = terminal::MAGENTA;
pub const PI: Color = terminal::CYAN;
pub const DOCTYPE: Color = terminal::CYAN;
#[allow(dead_code)]
pub const PUNCTUATION: Color = terminal::LIGHT_BLACK;
pub const LINE_NUMBER: Color = terminal::LIGHT_BLACK;
pub const SEARCH_MATCH_BG: Color = Color::C16(3); // yellow-ish background highlight

// The path header bar (screenwriter.rs's print_header_into_buffer) — a
// filled background color, not just colored text on the default
// background, so it reads as a distinct "bar" the same way the status
// bar's inverted style does, rather than just another highlighted line
// of content. Also reuses `terminal::BLUE`, which nothing else used yet.
pub const HEADER_BG: Color = terminal::BLUE;
pub const HEADER_FG: Color = terminal::WHITE;

pub fn style_for(value: &Value) -> Style {
    let fg = match value {
        Value::Text => TEXT,
        Value::CData => CDATA,
        Value::Comment => COMMENT,
        Value::ProcessingInstruction => PI,
        Value::DocType => DOCTYPE,
        Value::EmptyElement { .. } | Value::OpenElement { .. } | Value::CloseElement { .. } => TAG,
    };
    Style::fg(fg)
}
