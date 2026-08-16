//! Finding where a consumer pins a dated release.
//!
//! Measured against a real consumer repository rather than assumed. Five files
//! pin releases, in four different syntaxes, and one of them URL-encodes the
//! slash so the obvious grep silently finds nothing.

use std::fmt;

use crate::ids::ReleaseScheme;

/// How a site pins, which decides what repairing in place actually does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinKind {
    /// `rev = "..."`, a direct reference, or a lockfile's resolved commit.
    /// Repairing the release in place does not reach the consumer until the pin
    /// is edited or the lock is refreshed.
    Frozen,
    /// `branch = "..."`. Repairing in place does reach the consumer on the next
    /// resolve, so a new dated name is not required.
    Follows,
}

impl fmt::Display for PinKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Frozen => "frozen",
            Self::Follows => "follows",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub file: String,
    pub line: usize,
    pub reference: String,
    pub kind: PinKind,
    pub locked: Option<String>,
    /// The line the pin was read from.
    ///
    /// Kept because a release name alone cannot say which repository it belongs to:
    /// these forks cut releases on the same dated scheme, so `release/2026-07-28`
    /// exists in several of them. A consumer's pin of one repo would otherwise
    /// satisfy a check about another, which reads as "not behind" when it is.
    pub source: String,
}

/// Every release reference in one file's text under the configured scheme.
///
/// Handles all four observed syntaxes:
///   `rev = "release/..."`, `branch = "release/..."`,
///   a direct reference `...git@release/...`, and a lockfile's
///   `?rev=release%2F...`.
pub fn scan(file: &str, text: &str, scheme: &ReleaseScheme) -> Vec<Pin> {
    let mut pins = Vec::new();
    for (index, line) in text.lines().enumerate() {
        // The lockfile writes the slash percent-encoded, so a scanner matching
        // only the plain form concludes the lockfile carries no pin. That is a
        // false negative on the exact question this command exists to answer.
        let decoded = line.replace("%2F", "/").replace("%2f", "/");
        let start = match scheme {
            ReleaseScheme::Dated => decoded.find("release/"),
            ReleaseScheme::Fixed(name) => {
                decoded.match_indices(name.as_str()).find_map(|(start, _)| {
                    let preceding = decoded[..start].chars().next_back();
                    if preceding.is_none_or(|character| {
                        matches!(character, '\"' | '\'' | '=' | '/' | '@' | ' ')
                    }) {
                        Some(start).filter(|start| {
                            decoded[*start..]
                                .chars()
                                .take_while(|character| {
                                    !character.is_whitespace()
                                        && !matches!(character, '"' | '\'' | ',' | '#' | ')')
                                })
                                .collect::<String>()
                                == name.as_str()
                        })
                    } else {
                        None
                    }
                })
            }
        };
        let Some(start) = start else {
            continue;
        };
        let reference: String = decoded[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | '#' | ')'))
            .collect();
        let is_valid = match scheme {
            ReleaseScheme::Dated => reference.len() > "release/".len(),
            ReleaseScheme::Fixed(name) => reference == name.as_str(),
        };
        if !is_valid {
            continue;
        }
        let end = start + reference.len();
        let locked = decoded[end..]
            .strip_prefix('#')
            .map(|suffix| {
                suffix
                    .chars()
                    .take_while(char::is_ascii_hexdigit)
                    .collect::<String>()
            })
            .filter(|commit| commit.len() >= 6);
        let lowered = decoded.to_lowercase();
        let kind = if lowered.contains("branch") {
            PinKind::Follows
        } else {
            PinKind::Frozen
        };
        pins.push(Pin {
            file: file.to_owned(),
            line: index + 1,
            reference,
            kind,
            locked,
            source: decoded.trim().to_owned(),
        });
    }
    pins
}

/// Files worth scanning in a consumer.
pub const PIN_FILES: &[&str] = &[
    "pyproject.toml",
    "uv.lock",
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
];

pub fn render(pins: &[Pin]) -> String {
    if pins.is_empty() {
        return "  no dated release pinned by this consumer".to_owned();
    }
    pins.iter()
        .map(|pin| {
            format!(
                "  {}:{} {} ({})",
                pin.file, pin.line, pin.reference, pin.kind
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    #[test]
    fn a_rev_pin_is_frozen() {
        let text = r#"work-infra = { git = "https://x/y", rev = "release/2026-07-28" }"#;
        let pins = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].reference, "release/2026-07-28");
        assert_eq!(pins[0].kind, PinKind::Frozen);
    }

    #[test]
    fn a_branch_pin_follows() {
        let text =
            r#"sandbox-runner = { git = "https://x/y.git", branch = "release/2026-07-28.2" }"#;
        let pins = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(pins[0].kind, PinKind::Follows);
        assert_eq!(pins[0].reference, "release/2026-07-28.2");
    }

    #[test]
    fn a_direct_reference_is_found() {
        let text = r#"    "sandbox-runner @ git+https://x/y.git@release/2026-07-28.2","#;
        let pins = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].reference, "release/2026-07-28.2");
    }

    #[test]
    fn a_lockfile_percent_encoded_pin_is_found() {
        // The trap: `grep "release/"` over a real lockfile returns zero matches,
        // because the slash is written `%2F`. A scanner that misses this reports
        // "not pinned" for a file that pins.
        let text = "url = \"https://x/y.git?rev=release%2F2026-07-28.2#548aaafb\"";
        let pins = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(pins.len(), 1, "percent-encoded pin was missed");
        assert_eq!(pins[0].reference, "release/2026-07-28.2");
        assert_eq!(pins[0].kind, PinKind::Frozen);
    }

    #[test]
    fn a_line_merely_mentioning_releases_is_not_a_pin() {
        assert!(
            scan(
                "README.md",
                "See release/ for details",
                &crate::ids::ReleaseScheme::Dated,
            )
            .is_empty()
        );
    }

    #[test]
    fn several_consumers_may_sit_on_different_dated_releases() {
        // Observed simultaneously in one repository, so a single boolean answer
        // per consumer would be wrong.
        let text = concat!(
            "a = { git = \"u\", branch = \"release/2026-07-28.2\" }\n",
            "b = { git = \"u\", branch = \"release/2026-07-20\" }\n"
        );
        let pins = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(pins.len(), 2);
        assert_ne!(pins[0].reference, pins[1].reference);
    }

    #[test]
    fn a_fixed_branch_pin_is_found_with_its_locked_commit() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let text = "url = \"https://forge.invalid/o/r.git?branch=integration#548aaafb99\"";
        let pins = scan("uv.lock", text, &fixed);

        assert_eq!(pins.len(), 1, "was: {pins:?}");
        assert_eq!(pins[0].reference, "integration");
        assert_eq!(pins[0].kind, PinKind::Follows);
        assert_eq!(pins[0].locked.as_deref(), Some("548aaafb99"));
    }

    #[test]
    fn a_repository_named_after_the_fixed_branch_still_yields_its_pin() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let text =
            "url = \"https://forge.invalid/o/integration.git?branch=integration#548aaafb99\"";
        let pins = scan("uv.lock", text, &fixed);

        assert_eq!(pins.len(), 1, "was: {pins:?}");
        assert_eq!(pins[0].reference, "integration");
        assert_eq!(pins[0].locked.as_deref(), Some("548aaafb99"));
    }

    #[test]
    fn a_word_containing_the_fixed_name_is_not_a_pin() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        assert!(scan("pyproject.toml", "mode = \"reintegration\"", &fixed).is_empty());
    }

    #[test]
    fn dated_scanning_is_unchanged_by_the_scheme_parameter() {
        let text = "url = \"https://x/y.git?rev=release%2F2026-07-28.2#548aaafb\"";
        let pins = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);

        assert_eq!(pins[0].reference, "release/2026-07-28.2");
        assert_eq!(pins[0].locked.as_deref(), Some("548aaafb"));
    }
}
