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
        state.mark(Path::new("/r"), true, true);
    });

    // Then: housekeeping cannot turn the successful persistence into an error
    assert!(result.is_ok());
    Ok(())
}

#[test]
fn missing_flag_fields_default_independently() -> anyhow::Result<()> {
    // Given: state written by a version before `guided` existed
    let home = tempfile::tempdir()?;
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("claude-code-s1.json"),
        r#"{"repos":{"/r":{"noticed":true}}}"#,
    )?;

    // When: the record is read
    let flags = SessionState::load(home.path(), "claude-code", "s1").repo(Path::new("/r"));

    // Then: the existing flag survives and the absent one defaults to false
    assert!(flags.noticed);
    assert!(!flags.guided);
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
                state.mark(Path::new("/a"), true, true);
            })
        });
        let second = scope.spawn(|| {
            start.wait();
            SessionState::update(home.path(), "claude-code", "s1", |state| {
                state.mark(Path::new("/b"), true, true);
            })
        });
        (first.join(), second.join())
    });
    assert!(first.is_ok_and(|result| result.is_ok()));
    assert!(second.is_ok_and(|result| result.is_ok()));

    // Then: neither writer loses the other writer's state
    let state = SessionState::load(home.path(), "claude-code", "s1");
    assert!(state.repo(Path::new("/a")).noticed);
    assert!(state.repo(Path::new("/b")).noticed);
    Ok(())
}

#[test]
fn delete_removes_state_and_tolerates_missing_files() -> anyhow::Result<()> {
    // Given: a persisted session record
    let home = tempfile::tempdir()?;
    SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.mark(Path::new("/r"), true, true);
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
