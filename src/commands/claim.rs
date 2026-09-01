//! Claim decisions: who owns a branch, whether a claim can be taken, and the
//! words each outcome prints. Consumed by `start` and `finish`, which are the
//! commands that actually take and release claims.
//!
//! Enforcement layer three. Advisory on purpose: layers one and two are
//! default-correct paths and detectors, and hard refusal waits for evidence
//! that advice was insufficient.
// allow: SIZE_OK: 295 lines - claim coordination keeps owner-resolution behavior beside branch claim outcomes.

use std::path::Path;

use crate::commands::hook::owner_for;
use crate::config::Registry;
use crate::ids::BranchTarget;
use crate::store::Store;

/// Who is claiming.
///
/// `KNIVES_OWNER` is what the `OpenCode` plugin injects. Claude Code instead
/// provides its session ID. When neither harness provides an identity, a managed
/// working directory can identify its active owner from knives state. The OS user
/// is the fallback for a human at a terminal.
/// A blank `KNIVES_OWNER` is a plugin bug, not an identity. Treating it as one
/// would let two agents share a claim. The same applies to `CLAUDE_CODE_SESSION_ID`.
pub fn current_owner(cwd: &Path) -> anyhow::Result<String> {
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(owner);
    }
    if let Some(owner) = std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(owner);
    }
    if let Some(owner) = owner_for(cwd)? {
        return Ok(owner);
    }
    Ok(std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()))
}

/// What `claim` decided, so the caller can render and exit without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Taken {
        key: String,
        owner: String,
    },
    AlreadyYours {
        key: String,
        why: String,
    },
    HeldByAnother {
        key: String,
        owner: String,
        why: String,
    },
    UnknownRepo {
        name: String,
        known: Vec<String>,
    },
}

pub fn decide(
    registry: &Registry,
    store: &Store,
    target: &BranchTarget,
    owner: &str,
) -> ClaimOutcome {
    if registry.get(&target.repo).is_none() {
        return ClaimOutcome::UnknownRepo {
            name: target.repo.to_string(),
            known: registry.names().map(|name| name.to_string()).collect(),
        };
    }
    let key = target.to_string();
    match store
        .claims(Some(&target.repo))
        .into_iter()
        .find(|c| c.branch == target.branch.as_str())
    {
        Some(held) if held.owner == owner => ClaimOutcome::AlreadyYours {
            key,
            why: held.why.clone(),
        },
        Some(held) => ClaimOutcome::HeldByAnother {
            key,
            owner: held.owner.clone(),
            why: held.why.clone(),
        },
        None => ClaimOutcome::Taken {
            key,
            owner: owner.to_owned(),
        },
    }
}

pub fn render(outcome: &ClaimOutcome) -> String {
    match outcome {
        ClaimOutcome::Taken { key, owner } => format!("claimed {key} for {owner}"),
        ClaimOutcome::AlreadyYours { key, why } => format!("{key} is already yours: {why}"),
        // Naming the holder and their reason is the point: the next agent needs
        // to know whether it is the same work before deciding anything.
        ClaimOutcome::HeldByAnother { key, owner, why } => {
            format!("{key} is already claimed by {owner}: {why}")
        }
        ClaimOutcome::UnknownRepo { name, known } => {
            format!("unknown repo {name}; known: {}", known.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{
        RepoEntry,
        test_support::{EnvironmentGuard, environment_lock},
    };

    fn registry() -> Registry {
        Registry {
            repos: [(
                "a-repo".to_owned(),
                RepoEntry {
                    path: PathBuf::from("/tmp/a-repo"),
                    upstream: "u".to_owned(),
                    origin: "o".to_owned(),
                    base: None,
                    release: None,
                    release_branch: None,
                    test_count_command: None,
                    consumers: Vec::new(),
                },
            )]
            .into(),
            ..Registry::default()
        }
    }

    fn store(dir: &std::path::Path) -> Store {
        Store::open(dir.join("state.json")).unwrap()
    }

    fn names() -> BranchTarget {
        BranchTarget::new(
            crate::ids::RepoName::new("a-repo"),
            crate::ids::BranchName::new("feat/alpha"),
        )
    }

    #[test]
    fn an_unclaimed_branch_in_a_known_repo_can_be_taken() {
        let dir = tempfile::tempdir().unwrap();
        let target = names();
        let outcome = decide(&registry(), &store(dir.path()), &target, "agent-one");
        assert!(matches!(outcome, ClaimOutcome::Taken { .. }));
    }

    #[test]
    fn a_branch_held_by_another_agent_names_the_holder_and_the_reason() {
        // Given: agent-one holds the branch
        let dir = tempfile::tempdir().unwrap();
        let target = names();
        let mut subject = store(dir.path());
        let _ = subject.claim(&target, "agent-one", "fixing the parser");
        // When: agent-two asks
        let outcome = decide(&registry(), &subject, &target, "agent-two");
        // Then: it says who, and why, so the second agent can judge overlap
        let text = render(&outcome);
        assert!(text.contains("agent-one"), "was: {text}");
        assert!(text.contains("fixing the parser"), "was: {text}");
    }

    #[test]
    fn re_claiming_your_own_branch_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = names();
        let mut subject = store(dir.path());
        let _ = subject.claim(&target, "agent-one", "w");
        let outcome = decide(&registry(), &subject, &target, "agent-one");
        assert!(matches!(outcome, ClaimOutcome::AlreadyYours { .. }));
    }

    #[test]
    fn an_unknown_repo_is_reported_with_the_known_ones() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = decide(
            &registry(),
            &store(dir.path()),
            &BranchTarget::new(
                crate::ids::RepoName::new("nope"),
                crate::ids::BranchName::new("b"),
            ),
            "agent-one",
        );
        let text = render(&outcome);
        assert!(text.contains("nope"));
        assert!(text.contains("a-repo"));
    }

    #[test]
    fn current_owner_filters_a_blank_knives_owner() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "USER",
        ]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());
        environment.set("KNIVES_OWNER", "   ");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.set("USER", "terminal-user");
        assert_eq!(
            current_owner(Path::new("/tmp/unmanaged")).unwrap(),
            "terminal-user"
        );
    }

    #[test]
    fn current_owner_uses_the_session_id_when_knives_owner_is_absent() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);

        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
        environment.set("USER", "terminal-user");
        assert_eq!(
            current_owner(Path::new("/tmp/unmanaged")).unwrap(),
            "abc-123"
        );
    }

    #[test]
    fn current_owner_falls_back_to_user_when_the_session_id_is_blank() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "USER",
        ]);
        let config = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", config.path().to_str().unwrap());

        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "   ");
        environment.set("USER", "terminal-user");
        assert_eq!(
            current_owner(Path::new("/tmp/unmanaged")).unwrap(),
            "terminal-user"
        );
    }

    #[test]
    fn current_owner_uses_the_managed_directory_claim_when_harness_ids_are_absent() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
            "USER",
        ]);
        let home = tempfile::tempdir().unwrap();
        let repository = home.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        std::fs::write(
            home.path().join("repos.toml"),
            format!(
                "[repos.repo]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n",
                repository.display()
            ),
        )
        .unwrap();
        std::fs::write(
            home.path().join("state.json"),
            r#"{"claims":{"repo/feat/owner":{"repo":"repo","branch":"feat/owner","owner":"state-owner","why":"test","started":"2026-01-01T00:00:00Z","files":[]}}}"#,
        )
        .unwrap();
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.set("USER", "terminal-user");

        assert_eq!(current_owner(&repository).unwrap(), "state-owner");
    }

    #[test]
    #[should_panic(expected = "CLAUDE_CODE_SESSION_ID was not captured")]
    fn environment_guard_rejects_mutation_of_an_uncaptured_variable() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);

        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
    }
}
