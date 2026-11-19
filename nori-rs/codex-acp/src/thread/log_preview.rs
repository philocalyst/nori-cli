//! Bounded text for log lines.
//!
//! Codex events and messages run to tens of kilobytes. A log line that carries one whole wrote
//! hundreds of megabytes a day, so a line carries a short prefix of it.

use std::fmt;

/// The most characters of a value a log line carries.
const PREVIEW_CHARS: i32 = 200;

/// `value`'s `Debug` form, cut after [`PREVIEW_CHARS`] characters and marked with an ellipsis.
/// The value is formatted only as far as the cut, so a huge one costs no more than a small one.
pub(super) struct Preview<'a, T: fmt::Debug>(pub &'a T);

struct Capped {
    text: String,
    left: i32,
}

impl fmt::Write for Capped {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        for character in piece.chars() {
            if self.left == 0 {
                self.text.push('…');
                return Err(fmt::Error);
            }
            self.text.push(character);
            self.left -= 1;
        }
        Ok(())
    }
}

impl<T: fmt::Debug> fmt::Display for Preview<'_, T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut capped = Capped {
            text: String::new(),
            left: PREVIEW_CHARS,
        };
        // The write fails once the cap is reached; what fitted is what is shown.
        let _ = fmt::write(&mut capped, format_args!("{:?}", self.0));
        formatter.write_str(&capped.text)
    }
}

#[cfg(test)]
mod tests {
    use super::Preview;
    use pretty_assertions::assert_eq;

    #[test]
    fn short_values_are_shown_whole_and_long_ones_are_cut() {
        assert_eq!(Preview(&"short").to_string(), "\"short\"");
        let long = "x".repeat(10_000);
        let shown = Preview(&long).to_string();
        assert_eq!(shown.chars().count(), 201);
        assert!(shown.ends_with('…'));
    }
}
