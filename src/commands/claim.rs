//! Claim identity resolution.
//!
//! [`current_identity`] records both the claimant's name and how it was resolved.
//! The claim-lifecycle matrix introduced in Task 2 consumes that resolution kind.
//!
//! Enforcement layer three. Advisory on purpose: layers one and two are
//! default-correct paths and detectors, and hard refusal waits for evidence
//! that advice was insufficient.
// allow: SIZE_OK: claim coordination keeps identity-resolution behavior beside its tests.

use std::path::Path;

use crate::commands::hook::owner_for;
use crate::store::{Claim, OwnerKind};

/// A claimant and the source that established its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub owner: String,
    pub kind: OwnerKind,
}

/// The inputs relevant to taking or resuming a claim.
pub struct ClaimContext<'a> {
    pub held: Option<&'a Claim>,
    pub identity: &'a Identity,
    pub in_claimed_workspace: bool,
}

/// The mutually exclusive outcomes for a claim attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    Take,
    Resume { possession: bool },
    RefuseAnonymous,
    RefuseHeld,
}

/// Chooses the claim outcome from identity, possession, and the current claim.
pub fn decide(context: &ClaimContext<'_>) -> ClaimDecision {
    match context.held {
        None => ClaimDecision::Take,
        Some(_) if context.in_claimed_workspace => ClaimDecision::Resume { possession: true },
        Some(claim)
            if claim.kind == context.identity.kind
                && claim.kind != OwnerKind::OsUser
                && claim.owner == context.identity.owner =>
        {
            ClaimDecision::Resume { possession: false }
        }
        Some(claim)
            if claim.kind == OwnerKind::OsUser && context.identity.kind == OwnerKind::OsUser =>
        {
            ClaimDecision::RefuseAnonymous
        }
        Some(_) => ClaimDecision::RefuseHeld,
    }
}

/// Renders the complete claim context shared by refusals and other claim
/// lifecycle notices.
pub fn render_claim_context(
    claim: &Claim,
    last_seen: crate::seen::LastSeen,
    now: jiff::Timestamp,
) -> String {
    let claimed_age =
        crate::ledger::age(&claim.started, now).unwrap_or_else(|| "unknown".to_owned());
    format!(
        "{} is claimed by {} ({}), claimed {claimed_age} ago, {}: {}",
        claim.key(),
        claim.owner,
        owner_kind_label(claim.kind),
        render_last_seen(last_seen, now),
        claim.why,
    )
}

/// Renders a compact claim row for an advisory surface.
pub fn render_claim_line(
    subject: &str,
    claim: &Claim,
    last_seen: crate::seen::LastSeen,
    now: jiff::Timestamp,
) -> String {
    let claimed_age =
        crate::ledger::age(&claim.started, now).unwrap_or_else(|| "unknown".to_owned());
    format!(
        "{subject} ({}, {}, claimed {claimed_age} ago, {}): {}",
        claim.owner,
        owner_kind_label(claim.kind),
        render_last_seen(last_seen, now),
        claim.why,
    )
}

/// Renders the observation state without implying a liveness guarantee.
pub fn render_last_seen(last_seen: crate::seen::LastSeen, now: jiff::Timestamp) -> String {
    match last_seen {
        crate::seen::LastSeen::At(timestamp) => crate::ledger::age(&timestamp.to_string(), now)
            .map_or_else(
                || "not seen within the observation window".to_owned(),
                |age| format!("last seen {age} ago"),
            ),
        crate::seen::LastSeen::NoneSinceClaim => "no activity observed since claimed".to_owned(),
        crate::seen::LastSeen::NoneWithinWindow => {
            "not seen within the observation window".to_owned()
        }
    }
}

/// The durable token for a claim observation.
pub fn last_seen_provenance(last_seen: crate::seen::LastSeen) -> String {
    match last_seen {
        crate::seen::LastSeen::At(timestamp) => timestamp.to_string(),
        crate::seen::LastSeen::NoneSinceClaim => "none-since-claim".to_owned(),
        crate::seen::LastSeen::NoneWithinWindow => "none-within-window".to_owned(),
    }
}

/// The stable, human-readable identity-source label.
pub const fn owner_kind_label(kind: OwnerKind) -> &'static str {
    match kind {
        OwnerKind::HarnessSession => "harness-session",
        OwnerKind::WorkspaceDerived => "workspace-derived",
        OwnerKind::OsUser => "os-user",
    }
}

/// Resolves the identity that should own a claim.
///
/// `KNIVES_OWNER` is what the `OpenCode` plugin injects. Claude Code instead
/// provides its session ID. When neither harness provides an identity, a managed
/// working directory can identify its active owner from knives state. The OS user
/// is the fallback for a human at a terminal.
/// A blank `KNIVES_OWNER` is a plugin bug, not an identity. Treating it as one
/// would let two agents share a claim. The same applies to `CLAUDE_CODE_SESSION_ID`.
pub fn current_identity(cwd: &Path) -> anyhow::Result<Identity> {
    if let Some(owner) = std::env::var("KNIVES_OWNER")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Identity {
            owner,
            kind: OwnerKind::HarnessSession,
        });
    }
    if let Some(owner) = std::env::var("CLAUDE_CODE_SESSION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(Identity {
            owner,
            kind: OwnerKind::HarnessSession,
        });
    }
    if let Some(owner) = owner_for(cwd)? {
        return Ok(Identity {
            owner,
            kind: OwnerKind::WorkspaceDerived,
        });
    }
    Ok(Identity {
        owner: std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned()),
        kind: OwnerKind::OsUser,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use std::path::Path;

    use super::*;
    use crate::config::test_support::{EnvironmentGuard, environment_lock};
    use crate::store::{Claim, OwnerKind};

    fn held(owner: &str, kind: OwnerKind) -> Claim {
        Claim {
            repo: "a-repo".into(),
            branch: "feat/alpha".into(),
            owner: owner.into(),
            kind,
            why: "w".into(),
            started: "2026-01-01T00:00:00Z".into(),
            files: Vec::new(),
        }
    }

    fn identity(owner: &str, kind: OwnerKind) -> Identity {
        Identity {
            owner: owner.into(),
            kind,
        }
    }



    #[test]
    fn a_blank_knives_owner_falls_back_to_the_os_user() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        environment.set("KNIVES_OWNER", "   ");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Path::new("/tmp/unmanaged")).unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn a_harness_owner_resolves_as_a_harness_session() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        environment.set("KNIVES_OWNER", "agent-one");
        let identity = current_identity(Path::new("/tmp/unmanaged")).unwrap();

        assert_eq!(identity.owner, "agent-one");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn a_claude_code_session_resolves_as_a_harness_session() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Path::new("/tmp/unmanaged")).unwrap();

        assert_eq!(identity.owner, "abc-123");
        assert_eq!(identity.kind, crate::store::OwnerKind::HarnessSession);
    }

    #[test]
    fn a_blank_claude_code_session_falls_back_to_the_os_user() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        environment.remove("KNIVES_OWNER");
        environment.set("CLAUDE_CODE_SESSION_ID", "   ");
        environment.set("USER", "terminal-user");

        let identity = current_identity(Path::new("/tmp/unmanaged")).unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn the_user_fallback_is_anonymous() {
        let _lock = environment_lock();
        let environment =
            EnvironmentGuard::capture(&["KNIVES_OWNER", "CLAUDE_CODE_SESSION_ID", "USER"]);
        environment.remove("KNIVES_OWNER");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        environment.set("USER", "terminal-user");
        let identity = current_identity(Path::new("/tmp/unmanaged")).unwrap();

        assert_eq!(identity.owner, "terminal-user");
        assert_eq!(identity.kind, crate::store::OwnerKind::OsUser);
    }

    #[test]
    fn a_managed_directory_claim_resolves_as_workspace_derived() {
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

        let identity = current_identity(&repository).unwrap();

        assert_eq!(identity.owner, "state-owner");
        assert_eq!(identity.kind, crate::store::OwnerKind::WorkspaceDerived);
    }

    #[test]
    #[should_panic(expected = "CLAUDE_CODE_SESSION_ID was not captured")]
    fn environment_guard_rejects_mutation_of_an_uncaptured_variable() {
        let _lock = environment_lock();
        let environment = EnvironmentGuard::capture(&["KNIVES_OWNER"]);

        environment.set("CLAUDE_CODE_SESSION_ID", "abc-123");
    }
    #[test]
    fn an_unclaimed_branch_is_taken() {
        let identity = identity("agent-one", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: None,
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(decide(&context), ClaimDecision::Take);
    }

    #[test]
    fn the_same_harness_session_resumes() {
        let claim = held("agent-one", OwnerKind::HarnessSession);
        let identity = identity("agent-one", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(
            decide(&context),
            ClaimDecision::Resume { possession: false }
        );
    }

    #[test]
    fn a_different_harness_session_is_refused() {
        let claim = held("agent-one", OwnerKind::HarnessSession);
        let identity = identity("agent-two", OwnerKind::HarnessSession);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

    #[test]
    fn two_anonymous_owners_never_match_even_with_equal_strings() {
        let claim = held("terminal-user", OwnerKind::OsUser);
        let identity = identity("terminal-user", OwnerKind::OsUser);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseAnonymous);
    }

    #[test]
    fn possession_of_the_claimed_workspace_resumes_whatever_the_identity() {
        let claim = held("someone-else", OwnerKind::HarnessSession);
        let identity = identity("terminal-user", OwnerKind::OsUser);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: true,
        };

        assert_eq!(
            decide(&context),
            ClaimDecision::Resume { possession: true }
        );
    }

    #[test]
    fn mixed_kinds_with_equal_strings_are_strangers() {
        let claim = held("abc", OwnerKind::HarnessSession);
        let identity = identity("abc", OwnerKind::WorkspaceDerived);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

    #[test]
    fn matching_workspace_derived_identities_resume() {
        let claim = held("abc", OwnerKind::WorkspaceDerived);
        let identity = identity("abc", OwnerKind::WorkspaceDerived);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(
            decide(&context),
            ClaimDecision::Resume { possession: false }
        );
    }

    #[test]
    fn an_anonymous_challenger_to_a_harness_claim_is_refused_not_anonymous() {
        let claim = held("abc", OwnerKind::HarnessSession);
        let identity = identity("ubuntu", OwnerKind::OsUser);
        let context = ClaimContext {
            held: Some(&claim),
            identity: &identity,
            in_claimed_workspace: false,
        };

        assert_eq!(decide(&context), ClaimDecision::RefuseHeld);
    }

}
