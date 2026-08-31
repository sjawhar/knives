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
    /// Whether the reference belongs to the repository's release scheme.
    ///
    /// A consumer can pin a fork at an arbitrary tag or branch (`?rev=some-pin-tag`).
    /// That pin is a fact about the consumer — dropping it made the scan answer
    /// "does not pin" for a file that plainly pins, a confident absence on the exact
    /// question the consumers command exists to answer. Release-repair reasoning and
    /// skew comparison stay scoped to `on_scheme` pins; reporting shows everything.
    pub on_scheme: bool,
    /// The line the pin was read from.
    ///
    /// Kept because a release name alone cannot say which repository it belongs to:
    /// these forks cut releases on the same dated scheme, so `release/2026-07-28`
    /// exists in several of them. A consumer's pin of one repo would otherwise
    /// satisfy a check about another, which reads as "not behind" when it is.
    pub source: String,
}

/// How a consumer's pin relates to the newest published release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case", tag = "verdict")]
pub enum PinVerdict {
    /// Names the newest release; lock (when present) matches its commit.
    Current,
    /// Names the newest release but the locked commit is not the ref's commit.
    StaleLock { expected: String },
    /// Names an older release than the newest one.
    BehindName { newest: String },
    /// Names a release this fork's remote does not have.
    UnknownName,
    /// Pins the fork at a reference outside its release scheme (a tag or branch).
    /// A deliberate consumption choice, reported as a fact rather than a finding.
    OffScheme,
}

/// A malformed lock fragment that prevents a pin from being classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinProblem {
    pub file: String,
    pub line: usize,
    pub detail: String,
    pub source: String,
}

impl fmt::Display for PinProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.detail)
    }
}

/// Pins and parse problems found in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinScan {
    pub pins: Vec<Pin>,
    pub problems: Vec<PinProblem>,
}

/// Every release reference in one file's text under the configured scheme.
///
/// Handles all four observed syntaxes:
///   `rev = "release/..."`, `branch = "release/..."`,
///   a direct reference `...git@release/...`, and a lockfile's
///   `?rev=release%2F...`.
pub fn scan(file: &str, text: &str, scheme: &ReleaseScheme) -> PinScan {
    let mut pins = Vec::new();
    let mut problems = Vec::new();
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
            extend_off_scheme_pin(&mut pins, file, index, &decoded);
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
            extend_off_scheme_pin(&mut pins, file, index, &decoded);
            continue;
        }
        let source = decoded.trim().to_owned();
        let end = start + reference.len();
        let locked = match decoded[end..].strip_prefix('#') {
            Some(suffix) => {
                let fragment = suffix
                    .chars()
                    .take_while(|character| {
                        !character.is_whitespace() && !matches!(character, '"' | '\'' | ',' | ')')
                    })
                    .collect::<String>();
                if fragment.len() < 6
                    || !fragment
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                {
                    problems.push(PinProblem {
                        file: file.to_owned(),
                        line: index + 1,
                        detail: format!("malformed locked commit fragment `{fragment}`"),
                        source,
                    });
                    continue;
                }
                Some(fragment)
            }
            None => None,
        };
        let kind = if follows_branch_selector(&decoded[..start]) {
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
            on_scheme: true,
            source,
        });
    }
    PinScan { pins, problems }
}

/// Capture a git pin whose reference sits outside the release scheme.
///
/// Recognizes the same selector syntaxes the scheme path handles — a TOML
/// `rev = "..."` / `branch = "..."` and a lockfile's `?rev=...` — so a consumer
/// pinned at an arbitrary tag is still reported instead of reading as unpinned.
fn extend_off_scheme_pin(pins: &mut Vec<Pin>, file: &str, index: usize, decoded: &str) {
    let selectors: &[(&str, PinKind)] = &[
        ("?rev=", PinKind::Frozen),
        ("rev = \"", PinKind::Frozen),
        ("rev=\"", PinKind::Frozen),
        ("rev = '", PinKind::Frozen),
        ("branch = \"", PinKind::Follows),
        ("branch=\"", PinKind::Follows),
        ("branch = '", PinKind::Follows),
    ];
    let Some((start, kind)) = selectors.iter().find_map(|(marker, kind)| {
        decoded
            .find(marker)
            .map(|position| (position + marker.len(), *kind))
    }) else {
        return;
    };
    let reference: String = decoded[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | '#' | ')'))
        .collect();
    if reference.is_empty() {
        return;
    }
    let end = start + reference.len();
    let locked = decoded[end..].strip_prefix('#').and_then(|suffix| {
        let fragment: String = suffix
            .chars()
            .take_while(|character| {
                !character.is_whitespace() && !matches!(character, '"' | '\'' | ',' | ')')
            })
            .collect();
        (fragment.len() >= 6
            && fragment
                .chars()
                .all(|character| character.is_ascii_hexdigit()))
        .then_some(fragment)
    });
    pins.push(Pin {
        file: file.to_owned(),
        line: index + 1,
        reference,
        kind,
        locked,
        on_scheme: false,
        source: decoded.trim().to_owned(),
    });
}

fn follows_branch_selector(prefix: &str) -> bool {
    let prefix = prefix
        .trim_end()
        .trim_end_matches(&['"', '\''][..])
        .trim_end();
    let Some((before_value, _)) = prefix.rsplit_once('=') else {
        return false;
    };
    let key = before_value.trim_end();
    let Some(before_key) = key.strip_suffix("branch") else {
        return false;
    };
    before_key
        .chars()
        .next_back()
        .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
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
        let scanned = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(scanned.pins.len(), 1);
        assert_eq!(scanned.pins[0].reference, "release/2026-07-28");
        assert_eq!(scanned.pins[0].kind, PinKind::Frozen);
    }

    #[test]
    fn a_branch_pin_follows() {
        let text =
            r#"sandbox-runner = { git = "https://x/y.git", branch = "release/2026-07-28.2" }"#;
        let scanned = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(scanned.pins[0].kind, PinKind::Follows);
        assert_eq!(scanned.pins[0].reference, "release/2026-07-28.2");
    }

    #[test]
    fn a_direct_reference_is_found() {
        let text = r#"    "sandbox-runner @ git+https://x/y.git@release/2026-07-28.2","#;
        let scanned = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(scanned.pins.len(), 1);
        assert_eq!(scanned.pins[0].reference, "release/2026-07-28.2");
    }

    #[test]
    fn a_lockfile_percent_encoded_pin_is_found() {
        // The trap: `grep "release/"` over a real lockfile returns zero matches,
        // because the slash is written `%2F`. A scanner that misses this reports
        // "not pinned" for a file that pins.
        let text = "url = \"https://x/y.git?rev=release%2F2026-07-28.2#548aaafb\"";
        let scanned = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(scanned.pins.len(), 1, "percent-encoded pin was missed");
        assert_eq!(scanned.pins[0].reference, "release/2026-07-28.2");
        assert_eq!(scanned.pins[0].kind, PinKind::Frozen);
    }

    #[test]
    fn a_rev_selector_with_an_unrelated_branch_word_stays_frozen() {
        let text =
            r#"url = "https://x/y.git?rev=release%2F2026-07-28.2#548aaafb" # branch mentioned"#;
        let scanned = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);

        assert_eq!(scanned.pins.len(), 1, "was: {scanned:?}");
        assert_eq!(scanned.pins[0].kind, PinKind::Frozen);
    }

    #[test]
    fn a_line_merely_mentioning_releases_is_not_a_pin() {
        assert!(
            scan(
                "README.md",
                "See release/ for details",
                &crate::ids::ReleaseScheme::Dated,
            )
            .pins
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
        let scanned = scan("pyproject.toml", text, &crate::ids::ReleaseScheme::Dated);
        assert_eq!(scanned.pins.len(), 2);
        assert_ne!(scanned.pins[0].reference, scanned.pins[1].reference);
    }

    #[test]
    fn a_fixed_branch_pin_is_found_with_its_locked_commit() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let text = "url = \"https://forge.invalid/o/r.git?branch=integration#548aaafb99\"";
        let scanned = scan("uv.lock", text, &fixed);

        assert_eq!(scanned.pins.len(), 1, "was: {scanned:?}");
        assert_eq!(scanned.pins[0].reference, "integration");
        assert_eq!(scanned.pins[0].kind, PinKind::Follows);
        assert_eq!(scanned.pins[0].locked.as_deref(), Some("548aaafb99"));
    }

    #[test]
    fn a_repository_named_after_the_fixed_branch_still_yields_its_pin() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let text =
            "url = \"https://forge.invalid/o/integration.git?branch=integration#548aaafb99\"";
        let scanned = scan("uv.lock", text, &fixed);

        assert_eq!(scanned.pins.len(), 1, "was: {scanned:?}");
        assert_eq!(scanned.pins[0].reference, "integration");
        assert_eq!(scanned.pins[0].locked.as_deref(), Some("548aaafb99"));
    }

    #[test]
    fn a_word_containing_the_fixed_name_is_not_a_pin() {
        use crate::ids::{BranchName, ReleaseScheme};

        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        assert!(
            scan("pyproject.toml", "mode = \"reintegration\"", &fixed)
                .pins
                .is_empty()
        );
    }

    #[test]
    fn dated_scanning_is_unchanged_by_the_scheme_parameter() {
        let text = "url = \"https://x/y.git?rev=release%2F2026-07-28.2#548aaafb\"";
        let scanned = scan("uv.lock", text, &crate::ids::ReleaseScheme::Dated);

        assert_eq!(scanned.pins[0].reference, "release/2026-07-28.2");
        assert_eq!(scanned.pins[0].locked.as_deref(), Some("548aaafb"));
    }
    #[test]
    fn a_short_lock_fragment_is_a_parse_problem() {
        let scanned = scan(
            "uv.lock",
            "url = \"https://x/y.git?rev=release%2F2026-07-28.2#12345\"",
            &crate::ids::ReleaseScheme::Dated,
        );

        assert!(scanned.pins.is_empty());
        let problems = scanned
            .problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            problems,
            vec!["uv.lock:1: malformed locked commit fragment `12345`"]
        );
    }
}
