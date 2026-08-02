//! `knives claim` and `knives release-claim`: advisory coordination between agents.
//!
//! Enforcement layer three. Advisory on purpose: layers one and two are
//! default-correct paths and detectors, and hard refusal waits for evidence
//! that advice was insufficient.

use crate::cli::Exit;
use crate::config::{Registry, default_config_path, load};
use crate::ids::BranchTarget;
use crate::store::{Store, default_state_path};

/// Who is claiming.
///
/// `KNIVES_OWNER` is what the `OpenCode` plugin injects. Claude Code instead
/// provides its session ID. A claim cannot live in a shell environment variable:
/// each tool call is its own process and subagents are spawned by the harness, so
/// an `export` reaches nothing. The OS user is the fallback for a human at a terminal.
pub fn current_owner() -> String {
    std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("CLAUDE_CODE_SESSION_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_owned())
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

#[derive(Debug)]
pub struct ClaimRequest<'a> {
    pub target: BranchTarget,
    pub why: &'a str,
    pub fork_only: bool,
}

pub fn run_claim(request: &ClaimRequest<'_>) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let mut store = Store::open_for_update(default_state_path())?;
    let owner = current_owner();
    let outcome = decide(&registry, &store, &request.target, &owner);

    let exit = match &outcome {
        ClaimOutcome::Taken { .. } => {
            let _ = store.claim(&request.target, &owner, request.why);
            if request.fork_only {
                // Without the mark, a branch we deliberately keep with no
                // upstream pull request reads as an error in every report.
                store.mark_fork_only(&request.target, request.why);
            }
            store.save()?;
            Exit::Ok
        }
        ClaimOutcome::AlreadyYours { .. } => Exit::Ok,
        ClaimOutcome::HeldByAnother { .. } | ClaimOutcome::UnknownRepo { .. } => Exit::Usage,
    };

    match exit {
        Exit::Ok => println!("{}", render(&outcome)),
        _ => eprintln!("{}", render(&outcome)),
    }
    Ok(exit)
}

pub fn run_release(target: &BranchTarget, superseded_by: Option<&str>) -> anyhow::Result<Exit> {
    let mut store = Store::open_for_update(default_state_path())?;
    if !store.release_claim(target) {
        eprintln!("no claim on {target}");
        return Ok(Exit::Usage);
    }
    if let Some(replacement) = superseded_by {
        store.supersede(target, replacement);
    }
    store.save()?;
    println!("released {target}");
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::*;
    use crate::config::RepoEntry;

    struct EnvironmentGuard {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn capture(names: &[&'static str]) -> Self {
            Self {
                values: names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            }
        }

        fn set(name: &str, value: &str) {
            unsafe { std::env::set_var(name, value) };
        }

        fn remove(name: &str) {
            unsafe { std::env::remove_var(name) };
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

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
    fn the_owner_falls_back_when_the_injected_token_is_blank() {
        // A blank KNIVES_OWNER is a plugin bug, not an identity. Treating it as one
        // would let two agents share a claim.
        let _environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);
        EnvironmentGuard::set("KNIVES_OWNER", "   ");
        let owner = current_owner();
        assert_ne!(owner.trim(), "");
    }

    #[test]
    fn claude_session_id_is_the_owner_when_knives_owner_is_absent() {
        let _environment = EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID"]);
        EnvironmentGuard::remove("KNIVES_OWNER");
        EnvironmentGuard::set("CLAUDE_CODE_SESSION_ID", "abc-123");
        assert_eq!(current_owner(), "abc-123");
    }

    #[test]
    fn a_blank_claude_session_id_falls_back_to_the_user() {
        // Given: plugin identity is absent and Claude Code reports only whitespace.
        let _environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        EnvironmentGuard::remove("KNIVES_OWNER");
        EnvironmentGuard::set("CLAUDE_CODE_SESSION_ID", "   ");
        EnvironmentGuard::set("USER", "terminal-user");

        // When: a claim owner is chosen.
        let owner = current_owner();

        // Then: the user identity wins rather than a shared blank owner.
        assert_eq!(owner, "terminal-user");
    }
}
