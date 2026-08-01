use std::path::Path;
use std::sync::Barrier;

use super::SessionState;

#[cfg(unix)]
#[test]
fn an_unreadable_sibling_does_not_fail_update() {
    // Given: a broken symlink among the session records
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory).unwrap();
    std::os::unix::fs::symlink(directory.join("missing"), directory.join("broken")).unwrap();

    // When: a session state is persisted
    let result = SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.mark(Path::new("/r"), true, true);
    });

    // Then: housekeeping cannot turn the successful persistence into an error
    assert!(result.is_ok());
}

#[test]
fn missing_flag_fields_default_independently() {
    // Given: state written by a version before `guided` existed
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("hook-sessions");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("claude-code-s1.json"),
        r#"{"repos":{"/r":{"noticed":true}}}"#,
    )
    .unwrap();

    // When: the record is read
    let flags = SessionState::load(home.path(), "claude-code", "s1").repo(Path::new("/r"));

    // Then: the existing flag survives and the absent one defaults to false
    assert!(flags.noticed);
    assert!(!flags.guided);
}

#[test]
fn concurrent_updates_for_one_session_preserve_both_repos() {
    // Given: two hook processes ready to update one session
    let home = tempfile::tempdir().unwrap();
    let start = Barrier::new(2);

    // When: they begin their updates concurrently
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            SessionState::update(home.path(), "claude-code", "s1", |state| {
                state.mark(Path::new("/a"), true, true);
            })
            .unwrap();
        });
        let second = scope.spawn(|| {
            start.wait();
            SessionState::update(home.path(), "claude-code", "s1", |state| {
                state.mark(Path::new("/b"), true, true);
            })
            .unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
    });

    // Then: neither writer loses the other writer's state
    let state = SessionState::load(home.path(), "claude-code", "s1");
    assert!(state.repo(Path::new("/a")).noticed);
    assert!(state.repo(Path::new("/b")).noticed);
}

#[test]
fn delete_removes_state_and_tolerates_missing_files() {
    // Given: a persisted session record
    let home = tempfile::tempdir().unwrap();
    SessionState::update(home.path(), "claude-code", "s1", |state| {
        state.mark(Path::new("/r"), true, true);
    })
    .unwrap();

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
}
