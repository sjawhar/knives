//! Passive observations of ownership and workspace activity.
//!
//! `seen.json` is intentionally separate from the claim store: it records
//! evidence that may describe activity, never an assertion that a claim remains
//! live. Its short write lock is best effort so a missing or contended sidecar
//! cannot make an ordinary knives command fail.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commands::claim::Identity;
use crate::commands::wip::workspace_for;
use crate::config::{default_config_path, load as load_registry};
use crate::ids::WorkspaceName;
use crate::jj::WorkspaceActivity;
use crate::store::{Claim, OwnerKind, StoreLock, default_state_path};

const PRUNE_AGE: jiff::SignedDuration = jiff::SignedDuration::from_hours(90 * 24);
const THROTTLE_AGE: jiff::SignedDuration = jiff::SignedDuration::from_secs(60);

/// Sidecar observations, rather than a second source of claim intent.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Seen {
    /// A name only identifies a claimant in conjunction with its source.
    #[serde(default)]
    pub owners: BTreeMap<OwnerKind, BTreeMap<String, String>>,
    /// `<repo>/<workspace-dir-name>` → newest RFC 3339 observation.
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
}

/// What the three observation streams can honestly report for one claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastSeen {
    At(jiff::Timestamp),
    NoneSinceClaim,
    NoneWithinWindow,
}

/// Records an invocation observation without ever making that invocation fail.
///
/// The owner source is part of the observation key; OS-user identities are
/// deliberately excluded because they would conflate every anonymous claimant.
/// A resolved jj workspace also contributes its registered repository and
/// workspace-directory name.
pub fn record_observation(cwd: &Path, identity: &Identity) {
    let owner =
        (identity.kind != OwnerKind::OsUser).then(|| (identity.kind, identity.owner.clone()));
    let workspace = workspace_key(cwd);
    if owner.is_none() && workspace.is_none() {
        return;
    }

    let path = seen_path();
    let Ok(_lock) = StoreLock::acquire(&path) else {
        return;
    };
    let Ok(mut seen) = read(&path) else {
        return;
    };
    let now = jiff::Timestamp::now();
    let Ok(prune_before) = now.checked_sub(PRUNE_AGE) else {
        return;
    };
    let Ok(fresh_after) = now.checked_sub(THROTTLE_AGE) else {
        return;
    };
    let mut changed = prune(&mut seen, prune_before);
    let timestamp = now.to_string();

    if let Some((kind, owner)) = owner
        && !is_fresh(
            seen.owners.get(&kind).and_then(|owners| owners.get(&owner)),
            fresh_after,
        )
    {
        seen.owners
            .entry(kind)
            .or_default()
            .insert(owner, timestamp.clone());
        changed = true;
    }
    if let Some(workspace) = workspace
        && !is_fresh(seen.workspaces.get(&workspace), fresh_after)
    {
        seen.workspaces.insert(workspace, timestamp);
        changed = true;
    }
    if changed {
        let _ = save(&path, &seen);
    }
}

/// Loads the sidecar without taking its write lock.
pub fn load() -> Seen {
    read(&seen_path()).unwrap_or_default()
}

/// Returns the newest observation for a claim, or the honest coverage state
/// when every observation stream is empty.
pub fn last_seen(claim: &Claim, activity: &WorkspaceActivity, seen: &Seen) -> LastSeen {
    let workspace = WorkspaceName::new(workspace_for(&claim.branch));
    let workspace_key = format!("{}/{}", claim.repo, workspace.as_str());
    let timestamps = [
        activity.moves.get(&workspace).copied(),
        seen.owners
            .get(&claim.kind)
            .and_then(|owners| owners.get(&claim.owner))
            .and_then(|timestamp| timestamp.parse().ok()),
        seen.workspaces
            .get(&workspace_key)
            .and_then(|timestamp| timestamp.parse().ok()),
    ];
    if let Some(newest) = timestamps.into_iter().flatten().max() {
        return LastSeen::At(newest);
    }

    let Ok(started) = claim.started.parse::<jiff::Timestamp>() else {
        return LastSeen::NoneWithinWindow;
    };
    let Ok(seen_horizon) = jiff::Timestamp::now().checked_sub(PRUNE_AGE) else {
        return LastSeen::NoneWithinWindow;
    };
    let operation_covers_claim = activity.horizon.is_none_or(|horizon| horizon <= started);
    if operation_covers_claim && started >= seen_horizon {
        LastSeen::NoneSinceClaim
    } else {
        LastSeen::NoneWithinWindow
    }
}

fn seen_path() -> PathBuf {
    default_state_path().with_file_name("seen.json")
}

fn workspace_key(cwd: &Path) -> Option<String> {
    let workspace = cwd
        .ancestors()
        .find(|directory| directory.join(".jj").is_dir())?;
    let registry = load_registry(&default_config_path()).ok()?;
    let fork = crate::bind::here(&registry, workspace).ok()?.ok()?;
    let name = workspace.file_name()?.to_str()?;
    Some(format!("{}/{name}", fork.name))
}

fn read(path: &Path) -> Result<Seen, ()> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Seen::default()),
        Err(_) => Err(()),
    }
}

fn save(path: &Path, seen: &Seen) -> Result<(), ()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|_| ())?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|_| ())?;
    serde_json::to_writer_pretty(&mut temporary, seen).map_err(|_| ())?;
    temporary.write_all(b"\n").map_err(|_| ())?;
    temporary.persist(path).map_err(|_| ())?;
    Ok(())
}

fn prune(seen: &mut Seen, oldest: jiff::Timestamp) -> bool {
    let mut pruned = false;
    seen.owners.retain(|_, owners| {
        let before = owners.len();
        owners.retain(|_, timestamp| {
            timestamp
                .parse::<jiff::Timestamp>()
                .is_ok_and(|timestamp| timestamp >= oldest)
        });
        pruned |= owners.len() != before;
        !owners.is_empty()
    });
    let before = seen.workspaces.len();
    seen.workspaces.retain(|_, timestamp| {
        timestamp
            .parse::<jiff::Timestamp>()
            .is_ok_and(|timestamp| timestamp >= oldest)
    });
    pruned || seen.workspaces.len() != before
}

fn is_fresh(timestamp: Option<&String>, fresh_after: jiff::Timestamp) -> bool {
    timestamp
        .and_then(|timestamp| timestamp.parse::<jiff::Timestamp>().ok())
        .is_some_and(|timestamp| timestamp >= fresh_after)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        reason = "fixture setup failures and assertions are test failures"
    )]

    use std::collections::BTreeMap;

    use super::{LastSeen, Seen, WorkspaceActivity, last_seen, load, record_observation};
    use crate::commands::claim::Identity;
    use crate::config::test_support::{EnvironmentGuard, environment_lock};
    use crate::ids::WorkspaceName;
    use crate::store::{Claim, OwnerKind, default_state_path};

    fn ts(raw: &str) -> jiff::Timestamp {
        raw.parse().expect("valid timestamp")
    }

    fn claim(owner: &str, kind: OwnerKind, started: &str) -> Claim {
        Claim {
            repo: "a".to_owned(),
            branch: "feat/x".to_owned(),
            owner: owner.to_owned(),
            kind,
            why: "work".to_owned(),
            started: started.to_owned(),
            files: Vec::new(),
        }
    }

    fn configured_workspace(home: &tempfile::TempDir) -> std::path::PathBuf {
        let root = home.path().join("feat-x");
        std::fs::create_dir_all(root.join(".jj")).expect("create workspace marker");
        std::fs::write(
            home.path().join("repos.toml"),
            "[repos.a]\nupstream = \"u\"\norigin = \"o\"\n",
        )
        .expect("write registry");
        root
    }

    #[test]
    fn last_seen_is_the_newest_of_the_three_streams() {
        let claim = claim(
            "agent-one",
            OwnerKind::HarnessSession,
            "2026-01-01T00:00:00Z",
        );
        let activity = WorkspaceActivity {
            moves: BTreeMap::from([(WorkspaceName::new("feat-x"), ts("2026-01-01T00:00:00Z"))]),
            horizon: None,
        };
        let seen = Seen {
            owners: BTreeMap::from([(
                OwnerKind::HarnessSession,
                BTreeMap::from([("agent-one".to_owned(), "2026-01-03T00:00:00Z".to_owned())]),
            )]),
            workspaces: BTreeMap::from([(
                "a/feat-x".to_owned(),
                "2026-01-02T00:00:00Z".to_owned(),
            )]),
        };

        assert_eq!(
            last_seen(&claim, &activity, &seen),
            LastSeen::At(ts("2026-01-03T00:00:00Z"))
        );
    }

    #[test]
    fn an_observation_of_the_same_string_under_another_kind_is_a_stranger() {
        let claim = claim(
            "agent-one",
            OwnerKind::HarnessSession,
            &jiff::Timestamp::now().to_string(),
        );
        let activity = WorkspaceActivity::default();
        let seen = Seen {
            owners: BTreeMap::from([(
                OwnerKind::WorkspaceDerived,
                BTreeMap::from([("agent-one".to_owned(), jiff::Timestamp::now().to_string())]),
            )]),
            workspaces: BTreeMap::new(),
        };

        assert_eq!(
            last_seen(&claim, &activity, &seen),
            LastSeen::NoneSinceClaim
        );
    }

    #[test]
    fn full_coverage_with_no_sighting_is_none_since_claim() {
        let claim = claim(
            "agent-one",
            OwnerKind::HarnessSession,
            &jiff::Timestamp::now().to_string(),
        );

        assert_eq!(
            last_seen(&claim, &WorkspaceActivity::default(), &Seen::default()),
            LastSeen::NoneSinceClaim
        );
    }

    #[test]
    fn an_exhausted_window_is_reported_as_a_window_not_as_never() {
        let bounded_claim = claim(
            "agent-one",
            OwnerKind::HarnessSession,
            "2026-01-01T00:00:00Z",
        );
        let activity = WorkspaceActivity {
            moves: BTreeMap::new(),
            horizon: Some(ts("2026-01-02T00:00:00Z")),
        };

        assert_eq!(
            last_seen(&bounded_claim, &activity, &Seen::default()),
            LastSeen::NoneWithinWindow
        );
        let expired_claim = claim(
            "agent-one",
            OwnerKind::HarnessSession,
            &jiff::Timestamp::now()
                .checked_sub(jiff::SignedDuration::from_hours(91 * 24))
                .expect("91 days is within Jiff's timestamp range")
                .to_string(),
        );
        assert_eq!(
            last_seen(
                &expired_claim,
                &WorkspaceActivity::default(),
                &Seen::default()
            ),
            LastSeen::NoneWithinWindow
        );
    }

    #[test]
    fn an_os_user_identity_is_never_recorded() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        let home = tempfile::tempdir().expect("create config home");
        environment.set(
            "KNIVES_CONFIG_HOME",
            home.path().to_str().expect("utf-8 path"),
        );
        let cwd = configured_workspace(&home);

        record_observation(
            &cwd,
            &Identity {
                owner: "terminal-user".to_owned(),
                kind: OwnerKind::OsUser,
            },
        );

        assert!(load().owners.is_empty());
    }

    #[test]
    fn a_fresh_observation_is_throttled() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        let home = tempfile::tempdir().expect("create config home");
        environment.set(
            "KNIVES_CONFIG_HOME",
            home.path().to_str().expect("utf-8 path"),
        );
        let cwd = configured_workspace(&home);
        let identity = Identity {
            owner: "agent-one".to_owned(),
            kind: OwnerKind::HarnessSession,
        };

        record_observation(&cwd, &identity);
        let path = default_state_path().with_file_name("seen.json");
        let first = std::fs::read_to_string(&path).expect("first observation persisted");
        record_observation(&cwd, &identity);
        let second = std::fs::read_to_string(&path).expect("second observation persisted");

        assert_eq!(first, second, "a fresh sighting must not rewrite seen.json");
    }

    #[test]
    fn recording_an_observation_prunes_entries_older_than_ninety_days() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        let home = tempfile::tempdir().expect("create config home");
        environment.set(
            "KNIVES_CONFIG_HOME",
            home.path().to_str().expect("utf-8 path"),
        );
        let cwd = configured_workspace(&home);
        let stale = jiff::Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_hours(91 * 24))
            .expect("91 days is within Jiff's timestamp range")
            .to_string();
        let path = default_state_path().with_file_name("seen.json");
        std::fs::write(
            &path,
            serde_json::to_string(&Seen {
                owners: BTreeMap::from([(
                    OwnerKind::HarnessSession,
                    BTreeMap::from([("stale-owner".to_owned(), stale.clone())]),
                )]),
                workspaces: BTreeMap::from([("a/stale-workspace".to_owned(), stale)]),
            })
            .expect("serialize stale observations"),
        )
        .expect("write stale observations");

        record_observation(
            &cwd,
            &Identity {
                owner: "agent-one".to_owned(),
                kind: OwnerKind::HarnessSession,
            },
        );

        let seen = load();
        assert!(
            !seen
                .owners
                .get(&OwnerKind::HarnessSession)
                .is_some_and(|owners| owners.contains_key("stale-owner")),
            "was: {seen:?}"
        );
        assert!(
            !seen.workspaces.contains_key("a/stale-workspace"),
            "was: {seen:?}"
        );
    }
}
