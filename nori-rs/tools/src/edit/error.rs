//! Why an edit could not be applied, rendered for the model.

use std::fmt;

use thiserror::Error;

use super::hint::MissHint;
use super::matcher::MatchTier;

/// One place `old_string` matched, for an ambiguity report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// 1-based line number.
    pub line: i64,
    pub text: String,
}

#[derive(Debug, Error)]
pub enum EditError {
    #[error(
        "old_string is empty. To create a new file give `edit` a path that does not exist, or \
         use `write`. To replace the whole of an existing file use `write`. To change part of a \
         file, put the text to replace in old_string."
    )]
    EmptyOldString,

    #[error(
        "old_string and new_string are identical, so there is nothing to change. Make new_string \
         the text you want the file to contain."
    )]
    Identical,

    #[error("{0}")]
    NoMatch(MissHint),

    #[error("{}", Ambiguous(*tier, sites))]
    Ambiguous { tier: MatchTier, sites: Vec<Site> },

    #[error(
        "Edit {index} of {total} failed, so nothing was written (multi_edit applies all edits or \
         none). Each edit is applied in order and sees the result of the earlier ones.\n{source}"
    )]
    InMulti {
        index: i64,
        total: i64,
        #[source]
        source: Box<EditError>,
    },

    #[error("multi_edit needs at least one edit in `edits`.")]
    NoEdits,
}

struct Ambiguous<'a>(MatchTier, &'a [Site]);

impl fmt::Display for Ambiguous<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(tier, sites) = self;
        write!(
            f,
            "old_string matches {} places ({}), so it is ambiguous and nothing was changed:",
            sites.len(),
            tier.describe()
        )?;
        for site in sites.iter().take(8) {
            write!(f, "\n  line {}: {}", site.line, site.text)?;
        }
        if sites.len() > 8 {
            write!(f, "\n  ... and {} more", sites.len() - 8)?;
        }
        write!(
            f,
            "\nTo edit one of them, add surrounding lines to old_string until it identifies a \
             single place. To change every occurrence, set replace_all to true."
        )
    }
}
