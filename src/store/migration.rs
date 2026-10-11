//! What `knives ledger migrate` reads from and changes in a state file an
//! older knives left: the branch statements it kept there, and the keys that
//! name a fork by the registry key it was known by then.
//!
//! Every other open refuses such a file ([`Store::open`]); only the opens
//! here read it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{State, Store, StoreError};
use crate::config::FormerNames;
use crate::lock::FileLock;
use crate::statement::{Statement, StatementKind};

/// The maps an older knives kept branch statements in, each keyed
/// `<repo>/<branch>`: a pull request, fork-only, superseded-by and
/// depends-on.
pub(super) const LEGACY_STATEMENTS: [&str; 4] =
    ["tracked_pulls", "fork_only", "superseded", "dependencies"];

/// One branch statement an older knives kept in the state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyStatement {
    /// The map that held it: one of [`LEGACY_STATEMENTS`].
    pub map: &'static str,
    /// `<repo>/<branch>`, as the older knives keyed it.
    pub key: String,
    pub statement: Statement,
}

/// A state-file map whose keys name a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateMap {
    /// `<repo>/<branch>`, and the claim's own `repo`.
    Claims,
    /// `<repo>#<number>`.
    CommentMarks,
    /// `<repo>#<number>`.
    PullStates,
    /// `<repo>/<number>`.
    ForeignParents,
    /// `<repo>/<file>`.
    Conventions,
    /// `<repo>`.
    PullHeads,
}

impl std::fmt::Display for StateMap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Claims => "claims",
            Self::CommentMarks => "comment_marks",
            Self::PullStates => "pull_states",
            Self::ForeignParents => "foreign_parents",
            Self::Conventions => "conventions",
            Self::PullHeads => "pull_heads",
        })
    }
}

/// One state-file key that named a fork by the registry key an older knives
/// kept it under, and the key `knives ledger migrate` moves it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Renamed {
    pub map: StateMap,
    pub from: String,
    pub to: String,
}

/// `text`, a fork's name followed by `separator` and the rest, with a former
/// name replaced by the name that fork is kept under now; `None` when `text`
/// starts with no former name.
///
/// A text that already starts with a current name is left alone, whatever
/// its first component reads: a registry key can spell another fork's owner
/// (`openchamber` is `openchamber/openchamber`), and a migrated key must not
/// be migrated again.
pub fn renamed(former: &FormerNames, text: &str, separator: char) -> Option<String> {
    let current = former.values().any(|now| {
        text.strip_prefix(now.as_str())
            .is_some_and(|rest| rest.starts_with(separator))
    });
    if current {
        return None;
    }
    let (name, rest) = text.split_once(separator)?;
    former
        .get(name)
        .map(|now| format!("{now}{separator}{rest}"))
}

impl Store {
    /// The state file an older knives left, which every other open refuses,
    /// for `knives ledger migrate`.
    ///
    /// A real migration passes the lock it took on the state file, held until
    /// the store drops, so no other writer saves over it between the read and
    /// the save. A dry run passes none and saves nothing: a look that writes
    /// nothing, not even the lock file. A save is atomic, so what it reads is
    /// one whole state file.
    pub(crate) fn open_to_migrate(
        path: PathBuf,
        lock: Option<FileLock>,
    ) -> Result<Self, StoreError> {
        Self::read(path, &[], lock)
    }

    /// Every branch statement an older knives kept in the state file, in map
    /// order: pull requests, then fork-only, superseded-by and depends-on.
    ///
    /// A depends-on list is one statement, comma-joined as the ledger carries
    /// it; an empty list states that the branch needs nothing. A map that does
    /// not have the shape the older knives wrote fails the read, so nothing is
    /// migrated from a file nobody can vouch for.
    pub fn legacy_statements(&self) -> Result<Vec<LegacyStatement>, StoreError> {
        #[derive(Deserialize)]
        struct Legacy {
            #[serde(default)]
            tracked_pulls: BTreeMap<String, u64>,
            #[serde(default)]
            fork_only: BTreeMap<String, String>,
            #[serde(default)]
            superseded: BTreeMap<String, String>,
            #[serde(default)]
            dependencies: BTreeMap<String, Vec<String>>,
        }
        let maps: serde_json::Map<String, serde_json::Value> = LEGACY_STATEMENTS
            .iter()
            .filter_map(|name| {
                self.state
                    .extra
                    .get(*name)
                    .map(|map| ((*name).to_owned(), map.clone()))
            })
            .collect();
        let legacy: Legacy =
            serde_json::from_value(serde_json::Value::Object(maps)).map_err(|source| {
                StoreError::Parse {
                    path: self.path.clone(),
                    source,
                }
            })?;
        let [pulls, fork_only, superseded, dependencies] = LEGACY_STATEMENTS;
        let stated = |map, key: String, kind, value: Option<String>| LegacyStatement {
            map,
            key,
            statement: Statement { kind, value },
        };
        Ok(legacy
            .tracked_pulls
            .into_iter()
            .map(|(key, number)| stated(pulls, key, StatementKind::Pull, Some(number.to_string())))
            .chain(
                legacy
                    .fork_only
                    .into_iter()
                    .map(|(key, why)| stated(fork_only, key, StatementKind::ForkOnly, Some(why))),
            )
            .chain(legacy.superseded.into_iter().map(|(key, replacement)| {
                stated(
                    superseded,
                    key,
                    StatementKind::Superseded,
                    Some(replacement),
                )
            }))
            .chain(legacy.dependencies.into_iter().map(|(key, required)| {
                let value = (!required.is_empty()).then(|| required.join(","));
                stated(dependencies, key, StatementKind::Depends, value)
            }))
            .collect())
    }

    /// Drop the maps [`Store::legacy_statements`] reads; whether there were any.
    pub fn drop_legacy_statements(&mut self) -> bool {
        let before = self.state.extra.len();
        self.state
            .extra
            .retain(|name, _| !LEGACY_STATEMENTS.contains(&name.as_str()));
        self.state.extra.len() != before
    }

    /// Move every key that names a fork by one of its `former` names, and
    /// every claim's own `repo` with it, to the name that fork is kept under
    /// now; each move made, and each refused because its new key is already
    /// taken, in map order. A refused move leaves both keys as they were.
    pub fn rename_forks(&mut self, former: &FormerNames) -> (Vec<Renamed>, Vec<Renamed>) {
        let (mut moved, mut refused) = (Vec::new(), Vec::new());
        for renamed in self.state.renames(former) {
            if self.state.apply(&renamed, former) {
                moved.push(renamed);
            } else {
                refused.push(renamed);
            }
        }
        (moved, refused)
    }
}

impl State {
    /// Every key that names a fork by one of its `former` names, with the key
    /// it moves to, in map order.
    ///
    /// A claim is renamed by the `repo` it records, not by parsing its key,
    /// since a branch name may hold a `/`.
    pub(super) fn renames(&self, former: &FormerNames) -> Vec<Renamed> {
        let keyed = |map: StateMap, keys: Vec<&String>, separator: char| {
            keys.into_iter()
                .filter_map(move |key| {
                    Some(Renamed {
                        map,
                        from: key.clone(),
                        to: renamed(former, key, separator)?,
                    })
                })
                .collect::<Vec<_>>()
        };
        let claims = self.claims.iter().filter_map(|(key, claim)| {
            let now = former.get(claim.repo.as_str())?;
            Some(Renamed {
                map: StateMap::Claims,
                from: key.clone(),
                to: format!("{now}/{}", claim.branch),
            })
        });
        let pull_heads = self.pull_heads.keys().filter_map(|key| {
            Some(Renamed {
                map: StateMap::PullHeads,
                from: key.clone(),
                to: former.get(key.as_str())?.to_string(),
            })
        });
        claims
            .chain(keyed(
                StateMap::CommentMarks,
                self.comment_marks.keys().collect(),
                '#',
            ))
            .chain(keyed(
                StateMap::PullStates,
                self.pull_states.keys().collect(),
                '#',
            ))
            .chain(keyed(
                StateMap::ForeignParents,
                self.foreign_parents.keys().collect(),
                '/',
            ))
            .chain(keyed(
                StateMap::Conventions,
                self.conventions.keys().collect(),
                '/',
            ))
            .chain(pull_heads)
            .collect()
    }

    /// Move one value from `renamed.from` to `renamed.to`, the claim's own
    /// `repo` with it; `false`, moving nothing, when `to` is already taken.
    fn apply(&mut self, renamed: &Renamed, former: &FormerNames) -> bool {
        fn rekey<V>(map: &mut BTreeMap<String, V>, renamed: &Renamed) -> bool {
            if map.contains_key(&renamed.to) {
                return false;
            }
            if let Some(value) = map.remove(&renamed.from) {
                let _ = map.insert(renamed.to.clone(), value);
            }
            true
        }
        match renamed.map {
            StateMap::Claims => {
                let moved = rekey(&mut self.claims, renamed);
                if moved
                    && let Some(claim) = self.claims.get_mut(&renamed.to)
                    && let Some(now) = former.get(claim.repo.as_str())
                {
                    claim.repo = now.to_string();
                }
                moved
            }
            StateMap::CommentMarks => rekey(&mut self.comment_marks, renamed),
            StateMap::PullStates => rekey(&mut self.pull_states, renamed),
            StateMap::ForeignParents => rekey(&mut self.foreign_parents, renamed),
            StateMap::Conventions => rekey(&mut self.conventions, renamed),
            StateMap::PullHeads => rekey(&mut self.pull_heads, renamed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statements_an_older_version_kept_read_back_one_per_statement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"tracked_pulls":{"a-repo/feat/alpha":4545},"fork_only":{"a-repo/ci":"why"},"superseded":{"a-repo/old":"feat/alpha"},"dependencies":{"a-repo/feat/alpha":["a-repo#7","b#9"],"a-repo/none":[]}}"#,
        )
        .unwrap();
        let mut subject = Store::open_to_migrate(path, None).unwrap();

        let stated = |key: &str, kind, value: Option<&str>| LegacyStatement {
            map: match kind {
                StatementKind::Pull => "tracked_pulls",
                StatementKind::ForkOnly => "fork_only",
                StatementKind::Superseded => "superseded",
                _ => "dependencies",
            },
            key: key.to_owned(),
            statement: Statement {
                kind,
                value: value.map(str::to_owned),
            },
        };
        assert_eq!(
            subject.legacy_statements().unwrap(),
            [
                stated("a-repo/feat/alpha", StatementKind::Pull, Some("4545")),
                stated("a-repo/ci", StatementKind::ForkOnly, Some("why")),
                stated("a-repo/old", StatementKind::Superseded, Some("feat/alpha")),
                stated(
                    "a-repo/feat/alpha",
                    StatementKind::Depends,
                    Some("a-repo#7,b#9")
                ),
                stated("a-repo/none", StatementKind::Depends, None),
            ]
        );

        assert!(subject.drop_legacy_statements());
        assert!(subject.legacy_statements().unwrap().is_empty());
        assert!(!subject.drop_legacy_statements());
    }

    #[test]
    fn a_legacy_map_of_the_wrong_shape_fails_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"tracked_pulls":{"a-repo/feat/alpha":"soon"}}"#).unwrap();

        let error = Store::open_to_migrate(path, None)
            .unwrap()
            .legacy_statements()
            .unwrap_err();

        assert!(matches!(error, StoreError::Parse { .. }), "was: {error}");
    }
}
