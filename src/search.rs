// Regex search over rows. Adapted in spirit from jless's src/search.rs
// (~/git/jless/src/search.rs) — forward/reverse, wraps around, expands
// collapsed containers to reveal a match — but the mechanics differ
// because there's no single canonical string to regex over here (see
// docs/ARCHITECTURE.md §8.5, which flagged this as a "decide from
// benchmarks" item). v1 takes the simplest correct option from that
// section: match against each row's own text directly (tag name +
// attributes for elements, content for text/comment/CData/PI), doing a
// linear scan from the current row outward rather than precomputing a
// match list. This is O(distance to next match) per jump, not O(file
// size) upfront, but each row visited allocates a small string — fine
// for interactive use, not yet the "restrict to row-kind, no
// allocation" version §8.5 sketches as a fast-follow if this proves too
// slow on very large files.

use regex::Regex;

use crate::document::Document;
use crate::flatxml::{Index, Value};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

impl Direction {
    pub fn prompt_char(self) -> char {
        match self {
            Direction::Forward => '/',
            Direction::Reverse => '?',
        }
    }

    pub fn reversed(self) -> Direction {
        match self {
            Direction::Forward => Direction::Reverse,
            Direction::Reverse => Direction::Forward,
        }
    }
}

pub struct SearchState {
    pub term: String,
    pub direction: Direction,
    pub current_match: Option<Index>,
    regex: Option<Regex>,
}

impl Default for SearchState {
    fn default() -> Self {
        SearchState {
            term: String::new(),
            direction: Direction::Forward,
            current_match: None,
            regex: None,
        }
    }
}

fn searchable_text(doc: &Document, row: Index) -> String {
    match &doc.flat[row].value {
        Value::Text
        | Value::CData
        | Value::Comment
        | Value::ProcessingInstruction
        | Value::DocType => doc.row_text(row).to_string(),
        Value::EmptyElement { .. } | Value::OpenElement { .. } => {
            // Quote attribute values the same way they appear on screen
            // (lineprinter.rs's format_attrs) — searching for exactly
            // what you can see, e.g. `available="false"`, should work,
            // not just the unquoted value.
            let mut s = doc.row_text(row).to_string();
            for a in doc.flat.attrs_of(row) {
                s.push(' ');
                s.push_str(doc.text(a.name.clone()));
                s.push_str("=\"");
                s.push_str(doc.text(a.value.clone()));
                s.push('"');
            }
            s
        }
        Value::CloseElement { .. } => String::new(),
    }
}

impl SearchState {
    /// Compiles `term` as a regex and remembers the search direction.
    /// Returns an error message (for the status bar) on invalid regex.
    pub fn set_term(&mut self, term: String, direction: Direction) -> Result<(), String> {
        let regex = Regex::new(&term).map_err(|e| format!("invalid regex: {e}"))?;
        self.term = term;
        self.direction = direction;
        self.regex = Some(regex);
        Ok(())
    }

    pub fn has_term(&self) -> bool {
        self.regex.is_some()
    }

    pub fn is_match(&self, doc: &Document, row: Index) -> bool {
        match &self.regex {
            None => false,
            Some(re) => re.is_match(&searchable_text(doc, row)),
        }
    }

    /// Finds the next (or previous, depending on `direction`) matching
    /// row after `from`, wrapping around the whole document. Never
    /// returns `from` itself even if it matches (matches `n`/`N`'s "go to
    /// the *next* one" semantics rather than re-confirming the current
    /// position). Closing-tag rows are never match targets — their only
    /// text is a tag name that's redundant with the opening row.
    pub fn find(&self, doc: &Document, from: Index, direction: Direction) -> Option<Index> {
        let len = doc.flat.len() as Index;
        if len == 0 || self.regex.is_none() {
            return None;
        }
        let mut i = from;
        loop {
            i = match direction {
                Direction::Forward => {
                    if i + 1 >= len {
                        0
                    } else {
                        i + 1
                    }
                }
                Direction::Reverse => {
                    if i == 0 {
                        len - 1
                    } else {
                        i - 1
                    }
                }
            };
            if i == from {
                return None;
            }
            if !doc.flat[i].is_closing_of_container() && self.is_match(doc, i) {
                return Some(i);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, Source};
    use crate::xmlparser;

    fn doc(xml: &str) -> Document {
        let flat = xmlparser::parse(xml.as_bytes()).unwrap();
        Document {
            source: Source::Owned(xml.as_bytes().to_vec()),
            flat,
        }
    }

    #[test]
    fn test_find_forward_and_wrap() {
        let d = doc("<root><a>needle1</a><b>hay</b><c>needle2</c></root>");
        let mut s = SearchState::default();
        s.set_term("needle".to_string(), Direction::Forward)
            .unwrap();

        let m1 = s.find(&d, 0, Direction::Forward).unwrap();
        assert_eq!(d.row_text(m1), "needle1");

        let m2 = s.find(&d, m1, Direction::Forward).unwrap();
        assert_eq!(d.row_text(m2), "needle2");

        // Wraps back around to the first match.
        let m3 = s.find(&d, m2, Direction::Forward).unwrap();
        assert_eq!(m3, m1);
    }

    #[test]
    fn test_find_reverse() {
        let d = doc("<root><a>needle1</a><b>hay</b><c>needle2</c></root>");
        let mut s = SearchState::default();
        s.set_term("needle".to_string(), Direction::Forward)
            .unwrap();

        let last = d.flat.len() as Index - 1;
        let m = s.find(&d, last, Direction::Reverse).unwrap();
        assert_eq!(d.row_text(m), "needle2");
    }

    #[test]
    fn test_matches_tag_and_attrs() {
        let d = doc(r#"<root><widget kind="special"/></root>"#);
        let mut s = SearchState::default();
        s.set_term("special".to_string(), Direction::Forward)
            .unwrap();
        assert!(s.find(&d, 0, Direction::Forward).is_some());
    }

    #[test]
    fn test_invalid_regex_errors() {
        let mut s = SearchState::default();
        assert!(s
            .set_term("(unclosed".to_string(), Direction::Forward)
            .is_err());
    }
}
