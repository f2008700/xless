// Ported from jless's src/types.rs — generic terminal-size struct, no
// JSON/XML-specific knowledge.

#[derive(Copy, Clone, Debug, Default)]
pub struct TTYDimensions {
    pub width: u16,
    pub height: u16,
}

impl TTYDimensions {
    pub fn without_status_bar(&self) -> TTYDimensions {
        TTYDimensions {
            width: self.width,
            height: self.height.saturating_sub(1),
        }
    }
}
