//! A trigram bloom filter per session: "could this session contain the word `cancel`?" in a
//! few hundred bytes of memory, answered without reading the session's text.
//!
//! The filter holds the lowercase character trigrams of everything the session said. A word of
//! three or more characters can occur only if every one of its trigrams is present, so a miss
//! is certain and a hit is verified against the text. Words shorter than a trigram are not
//! answered by the filter at all (see [`Bloom::may_contain`]).

const HASHES: u32 = 2;
/// Bits allotted per distinct trigram; more bits, fewer false candidates.
const BITS_PER_TRIGRAM: usize = 12;
const MIN_LOG2: u8 = 13;
const MAX_LOG2: u8 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    log2: u8,
    bits: Vec<u8>,
}

fn trigram_hash(a: char, b: char, c: char) -> u32 {
    let mix = |value: char, rotate: u32, factor: u32| {
        u32::from(value).rotate_left(rotate).wrapping_mul(factor)
    };
    let hash = mix(a, 0, 0x9E37_79B1) ^ mix(b, 11, 0x85EB_CA6B) ^ mix(c, 22, 0xC2B2_AE35);
    hash ^ (hash >> 15)
}

fn trigrams(text: &str) -> impl Iterator<Item = u32> {
    let characters: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    (0..characters.len().saturating_sub(2)).filter_map(move |at| {
        let window = &characters[at..at + 3];
        window
            .iter()
            .all(|character| !character.is_whitespace() && !character.is_control())
            .then(|| trigram_hash(window[0], window[1], window[2]))
    })
}

impl Bloom {
    /// A filter sized for the trigrams of `texts`.
    pub fn build<'a>(texts: impl Iterator<Item = &'a str>) -> Self {
        let mut distinct: Vec<u32> = texts.flat_map(trigrams).collect();
        distinct.sort_unstable();
        distinct.dedup();
        let wanted = (distinct.len() * BITS_PER_TRIGRAM)
            .max(1)
            .next_power_of_two();
        let log2 = u8::try_from(wanted.trailing_zeros())
            .unwrap_or(MAX_LOG2)
            .clamp(MIN_LOG2, MAX_LOG2);
        let mut bloom = Self {
            log2,
            bits: vec![0; 1 << (log2 - 3)],
        };
        for hash in distinct {
            bloom.insert(hash);
        }
        bloom
    }

    fn positions(&self, hash: u32) -> [usize; HASHES as usize] {
        let mask = (1usize << self.log2) - 1;
        let second = hash.rotate_left(16).wrapping_mul(0x27D4_EB2F);
        [hash as usize & mask, second as usize & mask]
    }

    fn insert(&mut self, hash: u32) {
        for bit in self.positions(hash) {
            self.bits[bit >> 3] |= 1 << (bit & 7);
        }
    }

    fn has(&self, hash: u32) -> bool {
        self.positions(hash)
            .iter()
            .all(|bit| self.bits[bit >> 3] & (1 << (bit & 7)) != 0)
    }

    /// Whether `word` may occur in the session. Always true for words under three characters,
    /// which have no trigram to test.
    pub fn may_contain(&self, word: &str) -> bool {
        trigrams(word).all(|hash| self.has(hash))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bits.len() + 1);
        out.push(self.log2);
        out.extend_from_slice(&self.bits);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&log2, bits) = bytes.split_first()?;
        ((MIN_LOG2..=MAX_LOG2).contains(&log2) && bits.len() == 1 << (log2 - 3)).then(|| Self {
            log2,
            bits: bits.to_vec(),
        })
    }
}
