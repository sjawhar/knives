//! The events `knives sync` writes when a pull request moves, and how a
//! reader recognises one: `#<number> merged`, `closed`, `reopened` and
//! `advanced to <head>`.
//!
//! The writer and the recogniser live together so they cannot drift: `sync`
//! writes with [`transition_text`] and asks [`transition_of`] whether the
//! ledger already holds one, and the sweep discards what
//! [`repeated_transitions`] finds another machine already committed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use super::{Entry, Kind};
use crate::ids::short_id;

/// How a tracked pull request moved since the last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullState {
    Unchanged,
    Advanced,
    /// Seen for the first time. A first sighting is recorded silently, like
    /// comment marks: the forge already holds the history, so a pull request
    /// that merged months before tracking started is `New`, not `Merged`.
    New,
    Merged,
    Closed,
    /// Recorded settled last time, open now.
    Reopened,
}

impl PullState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::New => "new",
            Self::Advanced => "advanced",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Reopened => "reopened",
        }
    }
}

impl fmt::Display for PullState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to record about a pull request that moved, and nothing for one that did not.
///
/// `unchanged` is the absence of an event, and `new` is a first sighting rather
/// than something that happened: recording either would fill a fork's history
/// with one line per pull request per run.
pub fn transition_text(number: u64, state: PullState, head: &str) -> Option<String> {
    match state {
        PullState::Merged => Some(format!("#{number} merged")),
        PullState::Closed => Some(format!("#{number} closed")),
        PullState::Advanced => Some(format!("#{number} advanced to {}", short_id(head))),
        PullState::Reopened => Some(format!("#{number} reopened")),
        PullState::Unchanged | PullState::New => None,
    }
}

/// The pull request a transition event is about: the number its text names,
/// for an event whose text is one [`transition_text`] writes. `None` for
/// every other entry.
pub fn transition_of(entry: &Entry) -> Option<u64> {
    if entry.kind != Kind::Event {
        return None;
    }
    let (number, what) = entry.text.strip_prefix('#')?.split_once(' ')?;
    let number = number.parse().ok()?;
    let transition = matches!(what, "merged" | "closed" | "reopened")
        || what
            .strip_prefix("advanced to ")
            .is_some_and(|head| !head.is_empty() && !head.contains(char::is_whitespace));
    transition.then_some(number)
}

/// Whether two transition events record the same transition: the same
/// stated pull request and the same text.
fn same_transition(one: &Entry, other: &Entry) -> bool {
    one.pr == other.pr && one.text == other.text
}

/// The files of `entries` (a fork's, in stamp order) that repeat a
/// transition another machine already committed, and that no ref carries
/// yet (`uncommitted`): what a sweep discards after it pulls.
///
/// Each pull request's transition events, in stamp order, fall into runs of
/// the same transition one after another, and a run is one transition
/// however many machines recorded it. In a run holding a committed entry,
/// every uncommitted entry is a repeat. A committed entry is never one: a
/// ledger repository's history is pure additions, so a repeat two machines
/// both committed stays. A run of uncommitted entries alone is this
/// machine's own record, and stays whole.
pub fn repeated_transitions<'a>(
    entries: &'a [(PathBuf, Entry)],
    uncommitted: &BTreeSet<PathBuf>,
) -> Vec<&'a Path> {
    let mut by_pull: BTreeMap<u64, Vec<(&Path, &Entry)>> = BTreeMap::new();
    for (path, entry) in entries {
        if let Some(number) = transition_of(entry) {
            by_pull
                .entry(number)
                .or_default()
                .push((path.as_path(), entry));
        }
    }
    let mut repeated = Vec::new();
    for transitions in by_pull.values() {
        for run in transitions.chunk_by(|(_, one), (_, other)| same_transition(one, other)) {
            if run.iter().any(|(path, _)| !uncommitted.contains(*path)) {
                repeated.extend(
                    run.iter()
                        .map(|(path, _)| *path)
                        .filter(|path| uncommitted.contains(*path)),
                );
            }
        }
    }
    repeated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(text: &str, pr: Option<u64>) -> Entry {
        Entry {
            ts: "2026-10-10T00:00:00Z".to_owned(),
            owner: "a-test".to_owned(),
            email: None,
            subject: Some("feat/alpha".to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr,
            parents: Vec::new(),
        }
    }

    fn files(texts: &[(&str, &str)]) -> Vec<(PathBuf, Entry)> {
        texts
            .iter()
            .map(|(name, text)| (PathBuf::from(name), event(text, Some(632))))
            .collect()
    }

    #[test]
    fn only_a_transition_event_names_a_pull_request() {
        assert_eq!(transition_of(&event("#632 merged", None)), Some(632));
        assert_eq!(
            transition_of(&event("#632 advanced to 0123456789ab", None)),
            Some(632)
        );
        assert_eq!(transition_of(&event("#632 reopened", None)), Some(632));
        // A note that happens to read like one is an agent's assertion, not
        // a transition this tool observed.
        let note = Entry {
            kind: Kind::Note,
            ..event("#632 merged", None)
        };
        assert_eq!(transition_of(&note), None);
        assert_eq!(
            transition_of(&event("#632 merged upstream too", None)),
            None
        );
        assert_eq!(transition_of(&event("stated as #632", None)), None);
    }

    #[test]
    fn an_uncommitted_repeat_of_a_committed_transition_is_discarded_and_nothing_committed_is() {
        let entries = files(&[
            ("a.md", "#632 merged"),
            ("b.md", "#632 merged"),
            ("c.md", "#632 merged"),
        ]);
        // b is committed (a peer's), a and c are this machine's.
        let uncommitted = BTreeSet::from([PathBuf::from("a.md"), PathBuf::from("c.md")]);
        assert_eq!(
            repeated_transitions(&entries, &uncommitted),
            [Path::new("a.md"), Path::new("c.md")]
        );
        // Two committed repeats stay: the history is pure additions.
        let committed = BTreeSet::new();
        assert!(repeated_transitions(&entries, &committed).is_empty());
        // A run nobody committed is this machine's own record.
        let all: BTreeSet<PathBuf> = entries.iter().map(|(path, _)| path.clone()).collect();
        assert!(repeated_transitions(&entries, &all).is_empty());
    }

    #[test]
    fn a_transition_separated_by_another_is_a_new_one() {
        let entries = files(&[
            ("a.md", "#632 closed"),
            ("b.md", "#632 reopened"),
            ("c.md", "#632 closed"),
        ]);
        let uncommitted = BTreeSet::from([PathBuf::from("c.md")]);
        assert!(repeated_transitions(&entries, &uncommitted).is_empty());
    }
}
