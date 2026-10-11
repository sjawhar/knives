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
//! forget wins like any other statement when it is the newest. `depends` is
//! the exception: its live list is every requirement stated since the newest
//! `depends` forget, so two machines that each add one keep both. Only an
//! entry's `statement` field states anything; an event whose prose reads
//! `stated as #1234` states nothing.

use std::collections::{BTreeMap, BTreeSet};

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
    /// comma-joined. The live list is every requirement each `depends`
    /// statement since the newest `depends` forget names, so a statement adds
    /// to it whatever order machines make them in.
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
/// forget, and win when it is the newest. A table with a field this knives
/// does not know states nothing, and the read names its file
/// ([`crate::ledger::Ledger::read`]).
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
    /// Checked as an entry file is read ([`crate::ledger::Ledger::read`]),
    /// beside its stamp: a statement whose value its kind cannot hold states
    /// nothing, and the read names its file, rather than a part of it being
    /// read. `fork-only` and `superseded` hold prose, so any text is theirs;
    /// a forget holds nothing and is always well formed.
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
    /// The live statement per subject and kind; a `None` value is a live
    /// forget.
    live: BTreeMap<String, BTreeMap<StatementKind, Live>>,
}

/// One live statement: its value, and the stamp a statement replacing it is
/// stamped after.
#[derive(Debug)]
struct Live {
    /// The newest stamp among the entries it was folded from; `None` for a
    /// hand-built entry whose stamp does not parse.
    ts: Option<jiff::Timestamp>,
    value: Option<String>,
}

/// Where an entry falls in the order statements are folded in: its stamp,
/// then its place in the slice, so between equal stamps the later one is
/// newer.
type Place = (Option<jiff::Timestamp>, usize);

impl Statements {
    /// Fold `entries` into the statements live now.
    ///
    /// Per subject and kind the greatest `ts` wins, compared as instants rather
    /// than as text, where `…03.5Z` sorts before `…03Z`. Between equal stamps
    /// the later entry wins, as it does on disk. A `depends` list is instead
    /// the union of every `depends` value newer than the newest `depends`
    /// forget, or that forget when nothing is newer. An entry without a subject
    /// is about the repository, not a branch, and states nothing here; neither
    /// does a kind this knives does not know ([`StatementKind::Unknown`]).
    /// Every entry [`crate::ledger::Ledger::read`] returns has a parseable
    /// stamp; a hand-built entry whose stamp does not parse orders before
    /// every one that does.
    pub fn from_entries(entries: &[Entry]) -> Self {
        let mut newest: BTreeMap<&str, BTreeMap<StatementKind, (Place, Option<&str>)>> =
            BTreeMap::new();
        let mut forgotten: BTreeMap<&str, Place> = BTreeMap::new();
        let mut required: BTreeMap<&str, Vec<(Place, &str)>> = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            let (Some(subject), Some(statement)) = (entry.subject.as_deref(), &entry.statement)
            else {
                continue;
            };
            let place = (entry.ts.parse::<jiff::Timestamp>().ok(), index);
            match (statement.kind, statement.value.as_deref()) {
                (StatementKind::Unknown, _) => {}
                (StatementKind::Depends, Some(list)) => {
                    required.entry(subject).or_default().push((place, list));
                }
                (StatementKind::Depends, None) => {
                    let held = forgotten.entry(subject).or_insert(place);
                    *held = (*held).max(place);
                }
                (kind, value) => {
                    let held = newest
                        .entry(subject)
                        .or_default()
                        .entry(kind)
                        .or_insert((place, value));
                    if place > held.0 {
                        *held = (place, value);
                    }
                }
            }
        }
        let mut live: BTreeMap<String, BTreeMap<StatementKind, Live>> = newest
            .into_iter()
            .map(|(subject, kinds)| {
                let kinds = kinds
                    .into_iter()
                    .map(|(kind, ((ts, _), value))| {
                        let value = value.map(str::to_owned);
                        (kind, Live { ts, value })
                    })
                    .collect();
                (subject.to_owned(), kinds)
            })
            .collect();
        let subjects: BTreeSet<&str> = forgotten.keys().chain(required.keys()).copied().collect();
        for subject in subjects {
            let forget = forgotten.get(subject).copied();
            let since: Vec<(Place, &str)> = required
                .get(subject)
                .into_iter()
                .flatten()
                .copied()
                .filter(|(place, _)| forget.is_none_or(|forget| *place > forget))
                .collect();
            let stated = match (since.iter().map(|(place, _)| *place).max(), forget) {
                (Some((ts, _)), _) => {
                    let union: BTreeSet<&str> =
                        since.iter().flat_map(|(_, list)| list.split(',')).collect();
                    let value = Some(union.into_iter().collect::<Vec<_>>().join(","));
                    Live { ts, value }
                }
                (None, Some((ts, _))) => Live { ts, value: None },
                (None, None) => continue,
            };
            live.entry(subject.to_owned())
                .or_default()
                .insert(StatementKind::Depends, stated);
        }
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
        self.live
            .get(branch)?
            .get(&kind)
            .map(|live| live.value.as_deref())
    }

    /// The stamp of the live statement of `kind` about `branch`: a statement
    /// that replaces it is stamped after it, whatever the writer's clock
    /// says ([`crate::ledger::Stated::replaces`]).
    pub fn stamp(&self, branch: &str, kind: StatementKind) -> Option<jiff::Timestamp> {
        self.live.get(branch)?.get(&kind)?.ts
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
    fn every_requirement_stated_since_the_newest_forget_is_live() {
        // Given: two machines' lists for one branch, neither built from the
        // other's, then a forget, then one more requirement.
        let first = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Depends,
            Some("other#1,other#2"),
        );
        let concurrent = stated(
            "2026-08-15T22:15:00Z",
            "feat/alpha",
            StatementKind::Depends,
            Some("other#3"),
        );
        let forget = stated(
            "2026-08-15T22:16:00Z",
            "feat/alpha",
            StatementKind::Depends,
            None,
        );
        let after = stated(
            "2026-08-15T22:17:00Z",
            "feat/alpha",
            StatementKind::Depends,
            Some("other#4"),
        );

        // Then: both lists are live in either arrival order, as one list
        // without repeats.
        for entries in [
            [first.clone(), concurrent.clone()],
            [concurrent.clone(), first.clone()],
        ] {
            assert_eq!(
                Statements::from_entries(&entries).depends("feat/alpha"),
                ["other#1", "other#2", "other#3"]
            );
        }
        // And: a forget drops everything stated before it, and only that.
        let forgotten = [first.clone(), concurrent.clone(), forget.clone()];
        let live = Statements::from_entries(&forgotten);
        assert!(live.depends("feat/alpha").is_empty());
        assert_eq!(
            live.stated("feat/alpha", StatementKind::Depends),
            Some(None)
        );
        assert_eq!(
            Statements::from_entries(&[after, forget, first, concurrent]).depends("feat/alpha"),
            ["other#4"]
        );
    }

    #[test]
    fn a_live_statement_keeps_the_stamp_a_replacement_is_stamped_after() {
        // Given: a pull statement, and two depends lists.
        let entries = [
            stated(
                "2026-08-15T22:14:03Z",
                "feat/alpha",
                StatementKind::Pull,
                Some("12"),
            ),
            stated(
                "2026-08-15T22:15:00Z",
                "feat/alpha",
                StatementKind::Depends,
                Some("other#1"),
            ),
            stated(
                "2026-08-15T22:14:00Z",
                "feat/alpha",
                StatementKind::Depends,
                Some("other#2"),
            ),
        ];

        // When / Then: each kind's stamp is its newest entry's, and a kind
        // never stated has none.
        let live = Statements::from_entries(&entries);
        let at = |text: &str| text.parse::<jiff::Timestamp>().ok();
        assert_eq!(
            live.stamp("feat/alpha", StatementKind::Pull),
            at("2026-08-15T22:14:03Z")
        );
        assert_eq!(
            live.stamp("feat/alpha", StatementKind::Depends),
            at("2026-08-15T22:15:00Z")
        );
        assert_eq!(live.stamp("feat/alpha", StatementKind::ForkOnly), None);
        assert_eq!(live.stamp("feat/beta", StatementKind::Pull), None);
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

    #[test]
    fn between_equal_stamps_the_later_entry_wins() {
        // Given: two statements stamped the same instant, spelled two ways.
        let first = stated(
            "2026-08-15T22:14:03Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1"),
        );
        let second = stated(
            "2026-08-15T22:14:03.000Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("2"),
        );

        // When / Then: whichever comes later in the slice, the order entries
        // are on disk, is live.
        assert_eq!(
            Statements::from_entries(&[first.clone(), second.clone()]).pull("feat/alpha"),
            Some(2)
        );
        assert_eq!(
            Statements::from_entries(&[second, first]).pull("feat/alpha"),
            Some(1)
        );
    }

    #[test]
    fn a_stamp_that_does_not_parse_loses_to_every_one_that_does() {
        // Given: a statement whose stamp does not parse, and an older-looking
        // one whose stamp does.
        let unparseable = stated("not a time", "feat/alpha", StatementKind::Pull, Some("9"));
        let parsed = stated(
            "1970-01-01T00:00:00Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1"),
        );

        // When / Then: the parseable one is live in either order, and the
        // unparseable one only when it is the sole statement.
        for entries in [
            [unparseable.clone(), parsed.clone()],
            [parsed, unparseable.clone()],
        ] {
            assert_eq!(
                Statements::from_entries(&entries).pull("feat/alpha"),
                Some(1)
            );
        }
        assert_eq!(
            Statements::from_entries(&[unparseable]).pull("feat/alpha"),
            Some(9)
        );
    }
}
