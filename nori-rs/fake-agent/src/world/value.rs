//! Values a script is written in: durations, text, and what chunking text means.

use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

/// A length of time. Pkl evaluates `250.ms` to `{ value, unit }` and a bare number to seconds; JSON
/// may also say `"250ms"`, `"1.5s"`, `"2min"`, `"1h"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Span(pub Duration);

impl Span {
    pub const ZERO: Self = Self(Duration::ZERO);

    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// `quantity` of `unit` (`ns`, `us`, `ms`, `s`, `min`, `h`, `d`).
    pub fn parse_quantity(quantity: f64, unit: &str) -> Result<Self, String> {
        let nanos_per_unit = match unit {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "min" => 60e9,
            "h" => 3600e9,
            "d" => 86_400e9,
            other => {
                return Err(format!(
                    "unknown duration unit {other:?}; use ns, us, ms, s, min, h or d"
                ));
            }
        };
        let seconds = quantity * nanos_per_unit / 1e9;
        if !seconds.is_finite() || seconds < 0.0 {
            return Err(format!(
                "a duration must be finite and not negative, got {quantity}{unit}"
            ));
        }
        Ok(Self(Duration::from_secs_f64(seconds)))
    }

    fn parse_text(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let split = text
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let quantity: f64 = number
            .parse()
            .map_err(|_| format!("{text:?} is not a duration such as \"250ms\" or \"2min\""))?;
        Self::parse_quantity(quantity, if unit.is_empty() { "s" } else { unit.trim() })
    }
}

impl From<Duration> for Span {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Seconds(f64),
            Text(String),
            Quantity { value: f64, unit: String },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Seconds(seconds) => Span::parse_quantity(seconds, "s"),
            Raw::Text(text) => Span::parse_text(&text),
            Raw::Quantity { value, unit } => Span::parse_quantity(value, &unit),
        }
        .map_err(serde::de::Error::custom)
    }
}

impl Serialize for Span {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0.as_secs_f64())
    }
}

/// Text a script emits: a literal, or a generated block for huge outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Text {
    Plain(String),
    /// `unit` repeated `times` times, joined by `separator`; `{{n}}` in `unit` is the 1-based
    /// repetition. Lets a script produce megabytes of output without writing them down.
    Generated {
        unit: String,
        times: i64,
        #[serde(default)]
        separator: String,
    },
}

impl Default for Text {
    fn default() -> Self {
        Self::Plain(String::new())
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}

impl Text {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Plain(text) => text.is_empty(),
            Self::Generated { unit, times, .. } => unit.is_empty() || *times <= 0,
        }
    }

    /// The text, with generated blocks expanded. Variables are not substituted here.
    pub fn expand(&self) -> String {
        match self {
            Self::Plain(text) => text.clone(),
            Self::Generated {
                unit,
                times,
                separator,
            } => {
                let mut out = String::new();
                for n in 1..=(*times).max(0) {
                    if n > 1 {
                        out.push_str(separator);
                    }
                    out.push_str(&unit.replace("{{n}}", &n.to_string()));
                }
                out
            }
        }
    }
}

/// What a chunk is measured in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkUnit {
    #[default]
    Chars,
    Words,
    Lines,
}

/// How streamed text is cut and how fast it flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Chunking {
    /// Units per chunk. Zero sends the text as one chunk.
    pub size: i64,
    pub unit: ChunkUnit,
    /// Pause after each chunk but the last.
    pub every: Span,
}

impl Default for Chunking {
    fn default() -> Self {
        Self {
            size: 24,
            unit: ChunkUnit::Chars,
            every: Span::ZERO,
        }
    }
}

impl Chunking {
    /// `text` cut into chunks that concatenate back to it exactly.
    pub fn split(&self, text: &str) -> Vec<String> {
        let size = usize::try_from(self.size).unwrap_or_default();
        if size == 0 || text.is_empty() {
            return vec![text.to_owned()];
        }
        let mut chunks = Vec::new();
        match self.unit {
            ChunkUnit::Chars => {
                let mut current = String::new();
                let mut count = 0;
                for character in text.chars() {
                    current.push(character);
                    count += 1;
                    if count == size {
                        chunks.push(std::mem::take(&mut current));
                        count = 0;
                    }
                }
                if !current.is_empty() {
                    chunks.push(current);
                }
            }
            ChunkUnit::Words => {
                let mut current = String::new();
                let mut count = 0;
                for piece in text.split_inclusive(char::is_whitespace) {
                    current.push_str(piece);
                    count += 1;
                    if count == size {
                        chunks.push(std::mem::take(&mut current));
                        count = 0;
                    }
                }
                if !current.is_empty() {
                    chunks.push(current);
                }
            }
            ChunkUnit::Lines => {
                let mut current = String::new();
                let mut count = 0;
                for line in text.split_inclusive('\n') {
                    current.push_str(line);
                    count += 1;
                    if count == size {
                        chunks.push(std::mem::take(&mut current));
                        count = 0;
                    }
                }
                if !current.is_empty() {
                    chunks.push(current);
                }
            }
        }
        chunks
    }
}
