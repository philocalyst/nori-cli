//! A trigram bloom filter per session: "could this session contain the word `cancel`?" in a
//! few hundred bytes of memory, answered without reading the session's text.
//!
//! The filter holds the lowercase character trigrams of everything the session said. A word of
//! three or more characters can occur only if every one of its trigrams is present, so a miss
//! is certain and a hit is verified against the text. Words shorter than a trigram are not
//! answered by the filter at all (see [`Bloom::may_contain`]).
//!
//! A filter grows with its session without being written again: new trigrams set bits, and only
//! the bytes those bits fall in are patched in the file ([`Bloom::patch_file`]). A filter that
//! fills up is built again, larger, from the text; each doubling happens after the session
//! doubled its distinct trigrams, so the cost of building again per appended byte stays constant.

const HASHES: u32 = 2;
/// Bits allotted per distinct trigram; more bits, fewer false candidates.
const BITS_PER_TRIGRAM: usize = 12;
const MIN_LOG2: u8 = 13;
const MAX_LOG2: u8 = 24;
/// A filter with more than this share of its bits set (in percent) is built again, larger.
const CROWDED_PERCENT: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    log2: u8,
    bits: Vec<u8>,
    /// How many bits are set.
    ones: usize,
}

/// The bytes of a filter that new text set bits in, with the bits set: what to patch in its file.
pub(super) type Patch = std::collections::BTreeMap<usize, u8>;

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
            ones: 0,
        };
        let mut unused = Patch::new();
        for hash in distinct {
            bloom.insert(hash, &mut unused);
        }
        bloom
    }

    /// Add the trigrams of `text`, recording in `patch` which bytes changed.
    pub(super) fn add(&mut self, text: &str, patch: &mut Patch) {
        for hash in trigrams(text) {
            self.insert(hash, patch);
        }
    }

    /// Whether the filter is too full to answer well and should be built again, larger.
    pub(super) fn crowded(&self) -> bool {
        self.log2 < MAX_LOG2 && self.ones * 100 > (1usize << self.log2) * CROWDED_PERCENT
    }

    /// Bring the filter's file up to date with `patch`, which was recorded while adding to the
    /// filter this file holds: each changed byte is read, or-ed and written back, so nothing else
    /// is rewritten.
    pub(super) fn patch_file(&self, path: &std::path::Path, patch: &Patch) -> std::io::Result<()> {
        use std::io::Read;
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let mut header = [0u8; 1];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        if header[0] != self.log2 {
            return Err(std::io::Error::other("bloom file has another size"));
        }
        for (&at, &mask) in patch {
            let offset = u64::try_from(at + 1).map_err(std::io::Error::other)?;
            let mut byte = [0u8; 1];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut byte)?;
            byte[0] |= mask;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&byte)?;
        }
        Ok(())
    }

    fn positions(&self, hash: u32) -> [usize; HASHES as usize] {
        let mask = (1usize << self.log2) - 1;
        let second = hash.rotate_left(16).wrapping_mul(0x27D4_EB2F);
        [hash as usize & mask, second as usize & mask]
    }

    fn insert(&mut self, hash: u32, patch: &mut Patch) {
        for bit in self.positions(hash) {
            let mask = 1 << (bit & 7);
            if self.bits[bit >> 3] & mask == 0 {
                self.bits[bit >> 3] |= mask;
                self.ones += 1;
                *patch.entry(bit >> 3).or_default() |= mask;
            }
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
            ones: bits
                .iter()
                .map(|byte| usize::try_from(byte.count_ones()).unwrap_or(0))
                .sum(),
            bits: bits.to_vec(),
        })
    }
}
