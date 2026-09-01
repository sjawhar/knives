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
use crate::store::OwnerKind;

/// A claimant and the source that established its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub owner: String,
    pub kind: OwnerKind,
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
}
