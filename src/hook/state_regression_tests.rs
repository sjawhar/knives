use std::path::Path;
use std::sync::Barrier;

use super::SessionState;

#[cfg(unix)]
#[test]
fn an_unreadable_sibling_does_not_fail_update() -> anyhow::Result<()> {
    // Given: a broken symlink among the session records
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    std::os::unix::fs::symlink(directory.join("missing"), directory.join("broken"))?;

    // When: a session state is persisted
    let result = SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.mark_guided(Path::new("/r"));
    });

    // Then: housekeeping cannot turn the successful persistence into an error
    assert!(result.is_ok());
    Ok(())
}

#[test]
fn a_legacy_noticed_flag_does_not_suppress_notices() -> anyhow::Result<()> {
    // Given: state written by the previous one-boolean notice implementation.
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("claude-code-s1.json"),
        r#"{"repos":{"/r":{"noticed":true}}}"#,
    )?;

    // When: the record is read after digest-aware notices were introduced.
    let state = SessionState::load(home.path(), "claude-code", "s1");

    // Then: absent digest state is empty, allowing one harmless re-emission.
    assert!(!state.notice_seen(Path::new("/r"), "digest"));
    assert!(!state.repo(Path::new("/r")).guided);
    Ok(())
}

#[test]
fn a_document_predating_notice_digests_survives_update_and_reload() -> anyhow::Result<()> {
    // Given: state written before `seen_notices` existed.
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("claude-code-s1.json"),
        r#"{"repos":{"/r":{"noticed":true,"guided":true}}}"#,
    )?;

    // When: the legacy record is updated, then loaded through the public API.
    SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.record_notice(Path::new("/other"), "digest".to_owned());
    })?;
    let state = SessionState::load(home.path(), "claude-code", "s1");

    // Then: guidance survives, missing digest state defaults empty, and the
    // independent remote cache remains absent.
    assert!(state.repo(Path::new("/r")).guided);
    assert!(!state.notice_seen(Path::new("/r"), "digest"));
    assert!(state.remotes(Path::new("/r")).is_none());
    Ok(())
}

#[test]
fn a_legacy_owner_verdict_document_loads_without_reusing_its_verdict() -> anyhow::Result<()> {
    // Given: a session record written by the boolean owner-verdict cache.
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    let state_path = directory.join("claude-code-s1.json");
    std::fs::write(&state_path, r#"{"owner_verdicts":{"/r":true}}"#)?;

    // When: the session record is loaded and re-persisted.
    SessionState::update(home.path(), "claude-code", "s1", |_| {})?;
    let rewritten = std::fs::read_to_string(state_path)?;

    // Then: legacy verdicts are ignored rather than carried into the new cache shape.
    assert!(!rewritten.contains("owner_verdicts"));
    Ok(())
}

#[test]
fn concurrent_updates_for_one_session_preserve_both_repos() -> anyhow::Result<()> {
    // Given: two hook processes ready to update one session
    let home = tempfile::tempdir()?;
    let start = Barrier::new(2);

    // When: they begin their updates concurrently
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            SessionState::update(home.path(), "claude-code", "s1", |state| {
                state.record_notice(Path::new("/a"), "first".to_owned());
            })
        });
        let second = scope.spawn(|| {
            start.wait();
            SessionState::update(home.path(), "claude-code", "s1", |state| {
                state.record_notice(Path::new("/b"), "second".to_owned());
            })
        });
        (first.join(), second.join())
    });
    assert!(first.is_ok_and(|result| result.is_ok()));
    assert!(second.is_ok_and(|result| result.is_ok()));

    // Then: neither writer loses the other writer's state
    let state = SessionState::load(home.path(), "claude-code", "s1");
    assert!(state.notice_seen(Path::new("/a"), "first"));
    assert!(state.notice_seen(Path::new("/b"), "second"));
    Ok(())
}

#[test]
fn delete_removes_state_and_tolerates_missing_files() -> anyhow::Result<()> {
    // Given: a persisted session record
    let home = tempfile::tempdir()?;
    SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.mark_guided(Path::new("/r"));
    })?;

    // When: SessionEnd deletes it twice
    SessionState::delete(home.path(), "claude-code", "s1");
    SessionState::delete(home.path(), "claude-code", "s1");

    // Then: the session record is absent
    assert!(
        !home
            .path()
            .join("hook-sessions")
            .join("claude-code-s1.json")
            .exists()
    );
    Ok(())
}
