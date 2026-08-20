// Pairs a `FlatXml` with the source bytes its ranges point into. See
// README.md's Architecture section §8.1/§8.2: the source is mmap'd for real files
// (not copied into a `String`), and `FlatXml`'s `Row.range`s reference it
// directly, so this is the one place that owns both and can hand out
// `&str` slices safely.

use std::ops::Range;

use crate::flatxml::{FlatXml, Index};

pub enum Source {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Source {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Source::Mapped(m) => &m[..],
            Source::Owned(v) => &v[..],
        }
    }
}

pub struct Document {
    pub source: Source,
    pub flat: FlatXml,
}

impl Document {
    pub fn text(&self, range: Range<u32>) -> &str {
        let bytes = &self.source[range.start as usize..range.end as usize];
        // SAFETY / correctness: the whole source was validated as UTF-8
        // during parsing (xmlparser::parse), and every range we ever
        // construct is a sub-slice of byte-oriented tokens quick-xml gave
        // us from that same validated buffer, so this can't split a
        // multi-byte codepoint. Using the checked version here (not
        // from_utf8_unchecked) since the cost is negligible next to
        // terminal I/O and it's one less thing to get wrong.
        std::str::from_utf8(bytes).expect("row range was not valid UTF-8 — parser bug")
    }

    pub fn row_text(&self, index: Index) -> &str {
        self.text(self.flat[index].range.clone())
    }

    /// Same as `text`, but takes a plain `usize` byte range — for the
    /// editing code (edit.rs, app.rs), which works directly with the
    /// owned byte buffer (`Vec<u8>` is indexed by `usize`) rather than
    /// with `FlatXml` row ranges (`u32`, per ARCHITECTURE.md §8.3).
    pub fn text_range(&self, range: Range<usize>) -> &str {
        std::str::from_utf8(&self.source[range])
            .expect("byte range was not valid UTF-8 — edit produced a bad split?")
    }
}
