//! Chips: atomic tokens inside the text, drawn as labels.
//!
//! A chip stands for something that is not plain text where it sits: a large paste, an attached
//! image, a file or symbol reference, a setting to apply on send. In the buffer a chip is a single
//! character from Unicode's Supplementary Private Use Area-A (`U+F0000 + id`), so every motion,
//! deletion, yank, paste, undo and multi-selection edit treats it as one unit without knowing
//! anything about chips. The editor keeps each chip's label and tone; what the chip *means* (the
//! pasted text, the image path) belongs to whoever inserted it, keyed by its [ChipId].

use std::collections::BTreeMap;

use unicode_width::UnicodeWidthStr;

/// First codepoint of the range chips are encoded in.
const CHIP_BASE: u32 = 0xF_0000;
/// Last usable codepoint of the range (`U+FFFFE` and `U+FFFFF` are noncharacters).
const CHIP_LAST: u32 = 0xF_FFFD;

/// Identifies a chip within one editor.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChipId(u32);

impl ChipId {
    /// The character that stands for this chip in the text.
    pub fn as_char(self) -> char {
        char::from_u32(CHIP_BASE + self.0).unwrap_or(char::REPLACEMENT_CHARACTER)
    }

    /// The chip a character stands for, if it is one.
    pub fn from_char(ch: char) -> Option<Self> {
        let code = u32::from(ch);
        (CHIP_BASE..=CHIP_LAST)
            .contains(&code)
            .then(|| ChipId(code - CHIP_BASE))
    }
}

/// How a chip is coloured.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChipTone {
    /// Pasted content, attachments.
    #[default]
    Attachment,
    /// A reference to a file, symbol or skill.
    Reference,
    /// A setting that applies when the prompt is sent (model, effort, mode, agent, machine).
    Directive,
}

/// What the editor knows of a chip: how to draw it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chip {
    pub label: String,
    pub tone: ChipTone,
}

impl Chip {
    /// Columns the chip takes when drawn (at least one, so the cursor can sit on it).
    pub fn width(&self) -> usize {
        self.label.width().max(1)
    }
}

/// The chips of one editor.
#[derive(Clone, Debug, Default)]
pub struct Chips {
    next: u32,
    chips: BTreeMap<ChipId, Chip>,
}

impl Chips {
    /// Register a chip and return the character that stands for it.
    pub fn add(&mut self, chip: Chip) -> ChipId {
        let id = ChipId(self.next);
        self.next = (self.next + 1) % (CHIP_LAST - CHIP_BASE + 1);
        self.chips.insert(id, chip);
        id
    }

    pub fn get(&self, id: ChipId) -> Option<&Chip> {
        self.chips.get(&id)
    }

    /// The chip `ch` stands for, if it is a registered chip.
    pub fn of_char(&self, ch: char) -> Option<&Chip> {
        ChipId::from_char(ch).and_then(|id| self.chips.get(&id))
    }

    /// Forget chips that no longer appear in `text` (and are not in a register an undo or paste
    /// could bring back: chips are kept until the text they were in is replaced wholesale).
    pub fn clear(&mut self) {
        self.chips.clear();
    }

    /// Display width of `ch`: a chip's label width, or the character's own width.
    pub fn char_width(&self, ch: char) -> usize {
        match self.of_char(ch) {
            Some(chip) => chip.width(),
            None => unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0),
        }
    }
}

/// Every chip in `text`, with its byte range.
pub fn chips_in(text: &str) -> impl Iterator<Item = (ChipId, std::ops::Range<usize>)> + '_ {
    text.char_indices()
        .filter_map(|(at, ch)| ChipId::from_char(ch).map(|id| (id, at..at + ch.len_utf8())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_chip_is_one_character_and_back() {
        let mut chips = Chips::default();
        let id = chips.add(Chip {
            label: "[Pasted 2000 chars]".into(),
            tone: ChipTone::Attachment,
        });
        let text = format!("see {} please", id.as_char());
        let found: Vec<_> = chips_in(&text).collect();
        assert_eq!(found, vec![(id, 4..8)]);
        assert_eq!(chips.char_width(id.as_char()), 19);
        assert_eq!(ChipId::from_char('a'), None);
    }
}
