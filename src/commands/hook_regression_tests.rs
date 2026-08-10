use std::path::PathBuf;
use std::process::Command;

use super::match_with_trust;
use crate::config::{Registry, TrustRules};

fn registry_trusting(owner: &str) -> Registry {
    Registry {
        trust: TrustRules {
            roots: Vec::new(),
            owners: vec![owner.to_owned()],
        },
        ..Registry::default()
    }
}

fn checkout_declaring(owner: &str) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join("checkout");
    std::fs::create_dir_all(&root)?;

    let initialized = Command::new("git").args(["init"]).arg(&root).status()?;
    anyhow::ensure!(initialized.success(), "git init must create the checkout");

    let remote_added = Command::new("git")
        .args(["-C"])
        .arg(&root)
        .args([
            "remote",
            "add",
            "origin",
            &format!("https://forge.invalid/{owner}/repo.git"),
        ])
        .status()?;
    anyhow::ensure!(
        remote_added.success(),
        "git remote add must configure the owner"
    );

    std::fs::write(root.join("file.txt"), "content")?;
    Ok((directory, root))
}

#[test]
fn a_cached_owner_does_not_outlive_a_registry_owner_revocation() -> anyhow::Result<()> {
    // The fixture and the probe both spawn `git`; hold the environment lock so
    // a concurrent test legally mutating PATH cannot break the spawns.
    let _lock = crate::config::test_support::environment_lock();
    // Given: one session has cached a checkout whose remote owner is currently trusted.
    let home = tempfile::tempdir()?;
    let (_checkout, root) = checkout_declaring("old-owner")?;
    let cache = Some((home.path(), "opencode", "same-session"));
    assert!(
        match_with_trust(
            &[root.join("file.txt")],
            &registry_trusting("old-owner"),
            cache
        )?
        .is_some()
    );

    // When: the registry revokes that owner before the next hook event in the session.
    let result = match_with_trust(
        &[root.join("file.txt")],
        &registry_trusting("new-owner"),
        cache,
    )?;

    // Then: the cached checkout fact cannot preserve the revoked trust grant.
    assert!(result.is_none());
    Ok(())
}

#[test]
fn a_cached_owner_is_rechecked_when_the_registry_adds_the_owner() -> anyhow::Result<()> {
    let _lock = crate::config::test_support::environment_lock();
    // Given: one session cached a checkout while its remote owner was absent from the registry.
    let home = tempfile::tempdir()?;
    let (_checkout, root) = checkout_declaring("real-owner")?;
    let cache = Some((home.path(), "opencode", "same-session"));
    assert!(
        match_with_trust(
            &[root.join("file.txt")],
            &registry_trusting("other-owner"),
            cache
        )?
        .is_none()
    );

    // When: the registry adds the checkout's owner before the next hook event.
    let result = match_with_trust(
        &[root.join("file.txt")],
        &registry_trusting("real-owner"),
        cache,
    )?;

    // Then: the cached checkout fact is compared with the current registry.
    assert!(result.is_some());
    Ok(())
}
