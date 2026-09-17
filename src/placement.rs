//! The placement verdict: what a fork branch states before it exists.
//!
//! A fork changes somebody else's library, and most effects a consumer wants
//! from a library need no fork at all — a configuration value the library
//! exposes, an override in the consumer's own deployment tooling on the
//! resource the library creates, or the consumer's own code. A branch that
//! skips that question becomes a fork member nobody can explain, and then a
//! pull request upstream does not want. So a new branch states, before it is
//! created, the non-fork alternative it considered and why it rejected it,
//! and the statement rides in the ledger as a note the release verbs and the
//! `gh` passthrough read back.
//!
//! The verdict file is written by a placement red-team — an adversarial
//! reviewer whose job is to argue against the fork change; the `fork-work`
//! skill carries its brief. knives stores the file and reads one line of it:
//! the first, `verdict: CONSUMER | FORK | UPSTREAM`. The rest is prose for the
//! next reader (`alternative:`, `class:`, `judge:`, free text).

use crate::ledger::{Entry, Kind};

/// The text prefix that marks a ledger note as a placement verdict.
///
/// The ledger distinguishes note purposes by their text, not by a field
/// (`claimed: …`, `seized from …`); a placement note follows suit.
pub const NOTE_PREFIX: &str = "placement: ";

/// The refusal an upstream-side `include` gives a verdict that says the change
/// belongs in the consumer.
pub const CONSUMER_REFUSAL: &str = "the placement verdict says this belongs in the consumer";

/// Where the red-team ruled the change belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No fork: the consumer configures or overrides the library.
    Consumer,
    /// A fork member that rides the release and never becomes a pull request.
    Fork,
    /// A fork member that is also an upstream pull request.
    Upstream,
}

impl Verdict {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Consumer => "CONSUMER",
            Self::Fork => "FORK",
            Self::Upstream => "UPSTREAM",
        }
    }

    fn parse(token: &str) -> Option<Self> {
        match token {
            "CONSUMER" => Some(Self::Consumer),
            "FORK" => Some(Self::Fork),
            "UPSTREAM" => Some(Self::Upstream),
            _ => None,
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A verdict file as knives reads it: the ruling, and the whole text for the
/// ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub verdict: Verdict,
    pub text: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlacementError {
    #[error(
        "a placement verdict starts with `verdict: CONSUMER | FORK | UPSTREAM`; the first line was {0:?}"
    )]
    Verdict(String),
    #[error("a placement verdict is empty")]
    Empty,
}

impl Placement {
    /// Parse a verdict file. The first non-blank line decides; everything is kept.
    pub fn parse(text: &str) -> Result<Self, PlacementError> {
        let first = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or(PlacementError::Empty)?;
        let verdict = first
            .strip_prefix("verdict:")
            .and_then(|rest| Verdict::parse(rest.trim()))
            .ok_or_else(|| PlacementError::Verdict(first.to_owned()))?;
        Ok(Self {
            verdict,
            text: text.trim_end().to_owned(),
        })
    }

    /// The ledger note text: the marker, then the file as written.
    pub fn note_text(&self) -> String {
        format!("{NOTE_PREFIX}{}", self.text)
    }
}

/// The verdict the ledger records for `branch`: the newest placement note about
/// it, or nothing when no such note exists.
///
/// A note that carries the marker but no readable verdict is an error, not an
/// absence: a ledger the tool cannot read must not read as a ledger that says
/// nothing.
pub fn recorded(entries: &[Entry], branch: &str) -> Option<Result<Placement, PlacementError>> {
    entries
        .iter()
        .rev()
        .filter(|entry| entry.kind == Kind::Note && entry.subject.as_deref() == Some(branch))
        .find_map(|entry| entry.text.strip_prefix(NOTE_PREFIX))
        .map(Placement::parse)
}

/// Whether any recorded release composition named `branch` as a parent.
///
/// Grandfathering: the gate arrived with members already in releases and no
/// note behind them. A branch some cut or edit event recorded as a parent is an
/// existing member — moving it, or carrying it into a recut, keeps working —
/// and only a branch no composition has ever carried is a new inclusion.
pub fn composed(entries: &[Entry], branch: &str) -> bool {
    entries.iter().any(|entry| {
        entry
            .parents
            .iter()
            .any(|parent| parent.branches.iter().any(|named| named == branch))
    })
}

/// The question a new branch is refused with when it states no placement.
pub fn forcing_question(branch: &str) -> String {
    format!(
        "Starting a new branch in a fork. Are you sure there is no way to get this effect \
         without changing the upstream library, in a way that is not hacky? Consult the \
         placement red-team (skill fork-work) and pass its verdict: knives start {branch} \
         --placement <file>."
    )
}

/// The refusal a release verb gives a would-be member with no verdict behind it.
pub fn missing_member_refusal(branch: &str) -> String {
    format!(
        "{branch} carries no placement verdict; a fork member states the non-fork alternative \
         it rejected (knives start --placement)"
    )
}

/// The refusal `knives gh` gives an upstream pull request for a branch whose
/// verdict is not `UPSTREAM`.
pub fn not_upstream_refusal(branch: &str, verdict: Verdict) -> String {
    format!(
        "{branch} has placement verdict {verdict}, not UPSTREAM; an upstream pull request opens \
         only for a branch the placement red-team ruled UPSTREAM (knives start --placement)"
    )
}

/// Whether `branch` may become a member of a release, or why not.
///
/// Passes an existing member unasked (see [`composed`]); a new one needs a
/// recorded verdict that is not `CONSUMER`.
pub fn member_refusal(entries: &[Entry], branch: &str) -> Result<Option<String>, PlacementError> {
    if composed(entries, branch) {
        return Ok(None);
    }
    match recorded(entries, branch) {
        None => Ok(Some(missing_member_refusal(branch))),
        Some(Err(error)) => Err(error),
        Some(Ok(placement)) if placement.verdict == Verdict::Consumer => {
            Ok(Some(CONSUMER_REFUSAL.to_owned()))
        }
        Some(Ok(_)) => Ok(None),
    }
}

/// Whether `branch` may be opened as an upstream pull request, or why not.
///
/// No grandfathering here: an upstream pull request is a new act whatever the
/// branch's age, and the verdict is what says upstream wants it.
pub fn upstream_pull_refusal(
    entries: &[Entry],
    branch: &str,
) -> Result<Option<String>, PlacementError> {
    match recorded(entries, branch) {
        None => Ok(Some(missing_member_refusal(branch))),
        Some(Err(error)) => Err(error),
        Some(Ok(placement)) if placement.verdict != Verdict::Upstream => {
            Ok(Some(not_upstream_refusal(branch, placement.verdict)))
        }
        Some(Ok(_)) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::RecordedParent;

    fn note(subject: &str, text: &str) -> Entry {
        Entry {
            ts: "2026-09-17T10:00:00Z".to_owned(),
            owner: "agent".to_owned(),
            subject: Some(subject.to_owned()),
            kind: Kind::Note,
            disposition: None,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        }
    }

    fn cut_event(branches: &[&str]) -> Entry {
        Entry {
            ts: "2026-09-17T09:00:00Z".to_owned(),
            owner: "agent".to_owned(),
            subject: Some("release/2026-09-17".to_owned()),
            kind: Kind::Event,
            disposition: None,
            text: "cut".to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: branches
                .iter()
                .map(|branch| RecordedParent {
                    commit: "aaaa".to_owned(),
                    branches: vec![(*branch).to_owned()],
                })
                .collect(),
        }
    }

    #[test]
    fn the_first_non_blank_line_decides_the_verdict() {
        let placement = Placement::parse("\n  verdict: UPSTREAM \nalternative: none\n").unwrap();
        assert_eq!(placement.verdict, Verdict::Upstream);
        assert_eq!(placement.text, "\n  verdict: UPSTREAM \nalternative: none");
        assert_eq!(
            Placement::parse("alternative: x\nverdict: FORK"),
            Err(PlacementError::Verdict("alternative: x".to_owned()))
        );
        assert_eq!(
            Placement::parse("verdict: maybe"),
            Err(PlacementError::Verdict("verdict: maybe".to_owned()))
        );
        assert_eq!(Placement::parse("  \n"), Err(PlacementError::Empty));
    }

    #[test]
    fn the_newest_placement_note_wins_and_other_notes_are_ignored() {
        let entries = [
            note("feat/a", "placement: verdict: CONSUMER"),
            note("feat/a", "reviewed, looks fine"),
            note("feat/b", "placement: verdict: UPSTREAM"),
            note("feat/a", "placement: verdict: FORK\nalternative: none"),
        ];
        let newest = recorded(&entries, "feat/a").unwrap().unwrap();
        assert_eq!(newest.verdict, Verdict::Fork);
        assert!(recorded(&entries[1..2], "feat/a").is_none());
        assert!(recorded(&entries, "feat/c").is_none());
    }

    #[test]
    fn a_marked_note_without_a_verdict_is_an_error_not_an_absence() {
        let entries = [note("feat/a", "placement: whatever")];
        assert!(matches!(
            recorded(&entries, "feat/a"),
            Some(Err(PlacementError::Verdict(_)))
        ));
        assert!(member_refusal(&entries, "feat/a").is_err());
    }

    #[test]
    fn a_member_needs_a_verdict_that_is_not_consumer_unless_already_composed() {
        assert_eq!(
            member_refusal(&[], "feat/new").unwrap(),
            Some(missing_member_refusal("feat/new"))
        );
        assert_eq!(
            member_refusal(
                &[note("feat/new", "placement: verdict: CONSUMER")],
                "feat/new"
            )
            .unwrap(),
            Some(CONSUMER_REFUSAL.to_owned())
        );
        assert_eq!(
            member_refusal(&[note("feat/new", "placement: verdict: FORK")], "feat/new").unwrap(),
            None
        );
        // Grandfathered: a composition recorded it, so no note is asked for.
        assert_eq!(
            member_refusal(&[cut_event(&["feat/old"])], "feat/old").unwrap(),
            None
        );
        assert_eq!(
            member_refusal(&[cut_event(&["feat/old"])], "feat/new").unwrap(),
            Some(missing_member_refusal("feat/new"))
        );
    }

    #[test]
    fn an_upstream_pull_needs_upstream_and_no_age_excuses_it() {
        let composed = cut_event(&["feat/old"]);
        assert_eq!(
            upstream_pull_refusal(std::slice::from_ref(&composed), "feat/old").unwrap(),
            Some(missing_member_refusal("feat/old"))
        );
        assert_eq!(
            upstream_pull_refusal(
                &[composed, note("feat/old", "placement: verdict: FORK")],
                "feat/old"
            )
            .unwrap(),
            Some(not_upstream_refusal("feat/old", Verdict::Fork))
        );
        assert_eq!(
            upstream_pull_refusal(&[note("feat/x", "placement: verdict: UPSTREAM")], "feat/x")
                .unwrap(),
            None
        );
    }
}
