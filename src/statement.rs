//! What a person told knives about a branch that it cannot work out for itself.
//!
//! Four things: which pull request a branch belongs to, that it deliberately has
//! none upstream, which branch superseded it, and what it cannot land before.
//! Each is a [`Statement`] carried by a ledger [`Entry`], and [`Statements`]
//! folds a ledger into the statements that are live now.
//!
//! For one subject and kind, the entry with the newest stamp is the live
//! statement, whatever order the entries arrive in. A statement without a value
//! is a forget: the branch no longer has a statement of that kind, and the
//! forget wins like any other statement when it is the newest. Only an entry's
//! `statement` field states anything; an event whose prose reads
//! `stated as #1234` states nothing.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::Requirement;
use crate::ledger::Entry;

/// Which of the four statements an entry makes. Each kind fixes what
/// [`Statement::value`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatementKind {
    /// The pull request the branch belongs to: its number, in decimal.
    Pull,
    /// The branch deliberately has no upstream pull request: why.
    ForkOnly,
    /// The branch was superseded: the branch that replaced it.
    Superseded,
    /// What the branch cannot land before: every `<repo>#<number>` it needs,
    /// comma-joined. One statement carries the whole list, so stating it again
    /// replaces the list rather than adding to it.
    Depends,
    /// A kind a newer knives writes and this one does not know. Ledgers are
    /// shared between machines that upgrade at different times, so an entry
    /// stating one reads, and states nothing here, rather than making every
    /// read of its fork fail. Never written ([`crate::ledger::Ledger::append`]
    /// refuses it).
    #[serde(other)]
    Unknown,
}

impl std::fmt::Display for StatementKind {
    /// The kind as an entry file spells it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Pull => "pull",
            Self::ForkOnly => "fork-only",
            Self::Superseded => "superseded",
            Self::Depends => "depends",
            Self::Unknown => "unknown",
        })
    }
}

/// One statement about an entry's subject.
///
/// Written into an entry file as one inline table:
/// `statement = { kind = "pull", value = "1234" }`. Closed to other fields,
/// unlike the entry around it: a misspelt `value` would otherwise read as a
/// forget, and win when it is the newest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub kind: StatementKind,
    /// What [`StatementKind`] says it holds. `None` forgets: the subject no
    /// longer has a statement of this kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl Statement {
    /// Refuse a value its kind cannot hold, saying what it must be.
    ///
    /// Checked as an entry file is read
    /// ([`crate::ledger::Ledger::entries`]), beside its stamp, so a value
    /// written by hand or by a peer that a reader would have to skip fails
    /// the read instead: skipped, a newest malformed `pull` would hide the
    /// valid one before it, and a `depends` list read without its unreadable
    /// requirements would be written back without them by the next
    /// `knives depends`. `fork-only` and `superseded` hold prose, so any text
    /// is theirs; a forget holds nothing and is always well formed.
    pub fn check(&self) -> Result<(), String> {
        let Some(value) = self.value.as_deref() else {
            return Ok(());
        };
        let wanted = match self.kind {
            StatementKind::Pull => value
                .parse::<u64>()
                .is_err()
                .then_some("a `pull` statement holds one pull request number, in decimal"),
            StatementKind::Depends => value
                .split(',')
                .any(|text| Requirement::parse(text).is_none())
                .then_some(
                    "a `depends` statement holds every requirement as `<owner>/<name>#<number>`, \
                     comma-joined",
                ),
            StatementKind::ForkOnly | StatementKind::Superseded | StatementKind::Unknown => None,
        };
        wanted.map_or(Ok(()), |wanted| Err(format!("{wanted}; found {value:?}")))
    }
}

/// The statements live in a ledger, per branch.
#[derive(Debug, Default)]
pub struct Statements {
    /// The newest statement's value per subject and kind; a `None` value is a
    /// live forget.
    live: BTreeMap<String, BTreeMap<StatementKind, Option<String>>>,
}

impl Statements {
    /// Fold `entries` into the statements live now.
    ///
    /// Per subject and kind the greatest `ts` wins, compared as instants rather
    /// than as text, where `…03.5Z` sorts before `…03Z`. Between equal stamps
    /// the later entry wins, as it does on disk. An entry without a subject is
    /// about the repository, not a branch, and states nothing here; neither
    /// does a kind this knives does not know ([`StatementKind::Unknown`]).
    /// Every entry [`crate::ledger::Ledger::entries`] returns has a parseable
    /// stamp; a hand-built entry whose stamp does not parse orders before
    /// every one that does.
    pub fn from_entries(entries: &[Entry]) -> Self {
        type Newest<'a> = (Option<jiff::Timestamp>, Option<&'a str>);
        let mut newest: BTreeMap<&str, BTreeMap<StatementKind, Newest<'_>>> = BTreeMap::new();
        for entry in entries {
            let (Some(subject), Some(statement)) = (entry.subject.as_deref(), &entry.statement)
            else {
                continue;
            };
            if statement.kind == StatementKind::Unknown {
                continue;
            }
            let stated = (
                entry.ts.parse::<jiff::Timestamp>().ok(),
                statement.value.as_deref(),
            );
            newest
                .entry(subject)
                .or_default()
                .entry(statement.kind)
                .and_modify(|held| {
                    if stated.0 >= held.0 {
                        *held = stated;
                    }
                })
                .or_insert(stated);
        }
        let live = newest
            .into_iter()
            .map(|(subject, kinds)| {
                let kinds = kinds
                    .into_iter()
                    .map(|(kind, (_, value))| (kind, value.map(str::to_owned)))
                    .collect();
                (subject.to_owned(), kinds)
            })
            .collect();
        Self { live }
    }

    /// The pull request stated for `branch`. Every value read from an entry
    /// file is a decimal number ([`Statement::check`]).
    pub fn pull(&self, branch: &str) -> Option<u64> {
        self.value(branch, StatementKind::Pull)?.parse().ok()
    }

    /// Whether `branch` is stated to have no upstream pull request on purpose.
    pub fn fork_only(&self, branch: &str) -> bool {
        self.value(branch, StatementKind::ForkOnly).is_some()
    }

    /// The branch stated to have replaced `branch`.
    pub fn superseded_by(&self, branch: &str) -> Option<&str> {
        self.value(branch, StatementKind::Superseded)
    }

    /// Everything `branch` is stated to need first, as `<repo>#<number>`.
    /// Every part read from an entry file is one ([`Statement::check`]).
    pub fn depends(&self, branch: &str) -> Vec<String> {
        self.value(branch, StatementKind::Depends)
            .map_or_else(Vec::new, |list| {
                list.split(',').map(str::to_owned).collect()
            })
    }

    /// The live statement of `kind` about `branch`: `Some(None)` for a live
    /// forget, `None` when none of that kind was ever made.
    pub fn stated(&self, branch: &str, kind: StatementKind) -> Option<Option<&str>> {
        self.live.get(branch)?.get(&kind).map(Option::as_deref)
    }

    fn value(&self, branch: &str, kind: StatementKind) -> Option<&str> {
        self.stated(branch, kind).flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Kind;

    fn prose(ts: &str, subject: &str, text: &str) -> Entry {
        Entry {
            ts: ts.to_owned(),
            owner: "ses_fff688".to_owned(),
            email: None,
            subject: Some(subject.to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        }
    }

    fn stated(ts: &str, subject: &str, kind: StatementKind, value: Option<&str>) -> Entry {
        Entry {
            statement: Some(Statement {
                kind,
                value: value.map(str::to_owned),
            }),
            ..prose(ts, subject, "stated")
        }
    }

    #[test]
    fn the_newest_statement_wins_whatever_order_the_entries_arrive_in() {
        // Given: two pull statements for one branch, where the newer stamp has
        // a fraction and so sorts BEFORE the older one as text
        let older = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("999"),
        );
        let newer = stated(
            "2026-08-15T22:14:03.5Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1234"),
        );

        // When / Then: the newer instant is live in either arrival order
        for entries in [[older.clone(), newer.clone()], [newer, older]] {
            assert_eq!(
                Statements::from_entries(&entries).pull("feat/alpha"),
                Some(1234)
            );
        }
    }

    #[test]
    fn a_forget_is_a_statement_and_wins_when_it_is_newest() {
        let pull = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1234"),
        );
        let forget = stated(
            "2026-08-15T22:15:00Z",
            "feat/alpha",
            StatementKind::Pull,
            None,
        );

        // The newer forget leaves the branch with no stated pull request.
        assert_eq!(
            Statements::from_entries(&[pull.clone(), forget.clone()]).pull("feat/alpha"),
            None
        );

        // And: a forget older than the statement it would undo does nothing.
        let restated = Entry {
            ts: "2026-08-15T22:16:00Z".to_owned(),
            ..pull
        };
        assert_eq!(
            Statements::from_entries(&[forget, restated]).pull("feat/alpha"),
            Some(1234)
        );
    }

    #[test]
    fn only_a_structured_entry_states_anything_however_its_prose_reads() {
        // Given: an event whose body reads like a statement, newer than a real one
        let real = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1234"),
        );
        let prose_only = prose(
            "2026-08-15T22:15:00Z",
            "feat/alpha",
            "stated as #999; deliberately has no upstream pull request",
        );

        // Then: the prose neither states nor overrides
        let live = Statements::from_entries(&[real, prose_only.clone()]);
        assert_eq!(live.pull("feat/alpha"), Some(1234));
        assert!(!live.fork_only("feat/alpha"));

        // And: on its own it states nothing of any kind
        let alone = Statements::from_entries(&[prose_only]);
        assert_eq!(alone.pull("feat/alpha"), None);
        assert!(!alone.fork_only("feat/alpha"));
        assert_eq!(alone.superseded_by("feat/alpha"), None);
        assert!(alone.depends("feat/alpha").is_empty());
    }

    #[test]
    fn a_later_depends_restates_the_whole_list_rather_than_appending() {
        let first = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Depends,
            Some("other#1,other#2"),
        );
        let restated = stated(
            "2026-08-15T22:15:00Z",
            "feat/alpha",
            StatementKind::Depends,
            Some("other#3"),
        );

        assert_eq!(
            Statements::from_entries(std::slice::from_ref(&first)).depends("feat/alpha"),
            ["other#1", "other#2"]
        );
        assert_eq!(
            Statements::from_entries(&[first, restated]).depends("feat/alpha"),
            ["other#3"]
        );
    }

    #[test]
    fn each_subject_and_kind_holds_its_own_statement() {
        // A forget of one kind must not clear another kind, or another branch.
        let entries = [
            stated(
                "2026-08-15T22:14:01Z",
                "feat/alpha",
                StatementKind::Superseded,
                Some("feat/alpha-v2"),
            ),
            stated(
                "2026-08-15T22:14:02Z",
                "feat/alpha",
                StatementKind::ForkOnly,
                Some("CI we want here but not upstream"),
            ),
            stated(
                "2026-08-15T22:14:03Z",
                "feat/beta",
                StatementKind::Pull,
                Some("4545"),
            ),
            stated(
                "2026-08-15T22:14:04Z",
                "feat/alpha",
                StatementKind::Pull,
                None,
            ),
        ];

        let live = Statements::from_entries(&entries);
        assert_eq!(live.superseded_by("feat/alpha"), Some("feat/alpha-v2"));
        assert!(live.fork_only("feat/alpha"));
        assert_eq!(live.pull("feat/alpha"), None);
        assert_eq!(live.pull("feat/beta"), Some(4545));
        assert!(!live.fork_only("feat/beta"));
    }
}
