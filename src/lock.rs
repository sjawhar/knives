//! An advisory lock on a file, held for the duration of a read-modify-write.
//!
//! Without it a JSON sidecar is last-writer-wins: two writers that interleave
//! read, decide and write both see the old state, both report success, and the
//! second erases the first. The claim store and the two sidecars (sightings,
//! the hook's session state) all take one.

use std::fs::{File, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a writer waits for the lock and how it spaces its attempts.
///
/// Pauses grow so a long hold is not polled hundreds of times, and jitter
/// keeps waiters out of lockstep.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LockWait {
    pub deadline: Duration,
    pub floor: Duration,
    pub ceiling: Duration,
}

impl LockWait {
    /// One minute, for the claim store: `start` holds the lock for the whole
    /// workspace creation, so a waiter must outlast several holds.
    pub(crate) const CLAIM: Self = Self {
        deadline: Duration::from_secs(60),
        floor: Duration::from_millis(20),
        ceiling: Duration::from_secs(2),
    };

    /// One second, for the best-effort sidecar locks (`seen`, the hook's
    /// session state): a stale one must never stall a command or a hook event.
    pub(crate) const BRIEF: Self = Self {
        deadline: Duration::from_secs(1),
        floor: Duration::from_millis(20),
        ceiling: Duration::from_millis(200),
    };
}

/// The lock file a wait gave up on: the holder's pid when the file names one,
/// and the file's age either way.
#[derive(Debug)]
pub struct LockHolder {
    /// `None` for a lock file without a pid: one whose holder had not written
    /// its pid yet, or one written by something other than this lock.
    pub pid: Option<u32>,
    pub held_for: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}", held_message(path, holder))]
    Held { path: PathBuf, holder: LockHolder },
}

fn held_message(path: &Path, holder: &LockHolder) -> String {
    let secs = holder.held_for.as_secs();
    let who = holder.pid.map_or_else(
        || format!("holder unknown, lock written {secs}s ago"),
        |pid| format!("pid {pid}, holding for {secs}s"),
    );
    format!(
        "another knives command ({who}) is holding {}; try again in a moment",
        path.display()
    )
}

/// Uniform in `[floor, min(ceiling, floor * 2^attempt)]`: `random` is scaled
/// onto the span rather than reduced modulo it, so `0` is the floor and
/// `u64::MAX` the cap.
fn pause(attempt: u32, wait: LockWait, random: u64) -> Duration {
    let cap = wait
        .floor
        .saturating_mul(1 << attempt.min(16))
        .min(wait.ceiling);
    let span = u64::try_from(cap.saturating_sub(wait.floor).as_nanos()).unwrap_or(u64::MAX);
    let scaled = (u128::from(random) * (u128::from(span) + 1)) >> 64;
    let jitter = u64::try_from(scaled).unwrap_or(span);
    wait.floor + Duration::from_nanos(jitter)
}

/// Jitter source. The standard library's per-process randomised hasher is
/// enough to break lockstep between waiters, and costs no dependency.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// The lock file's first token is the holder's pid; a file without one has an
/// unknown holder but still an age. The waiter opened this file itself moments
/// ago, so a read or stat failure here is the filesystem failing and is
/// reported as that, not as a lock held by nobody.
fn read_holder(path: &Path) -> Result<LockHolder, std::io::Error> {
    let text = std::fs::read_to_string(path)?;
    let pid: Option<u32> = text
        .split_whitespace()
        .next()
        .and_then(|token| token.parse().ok());
    let held_for = std::fs::metadata(path)?
        .modified()?
        .elapsed()
        .unwrap_or_default();
    Ok(LockHolder { pid, held_for })
}

/// The operating system's advisory lock on `<stem>.lock` beside the file it
/// guards ([`File::try_lock`]), held by the open file handle: the kernel
/// releases it when the holder exits, however it exits — a SIGKILL from a
/// harness that timed a command out, a Ctrl-C, a panic — so a stale lock
/// cannot outlive a crashed writer and nobody has to remove a file. The file
/// itself is never unlinked: it is the shared inode every waiter locks, and
/// deleting it by path after an unlock would put two waiters on two different
/// inodes, each holding "the" lock. After acquiring, the holder writes its pid
/// into the file so a waiter that gives up can name who it waited on; the
/// file's mtime gives the age.
///
/// A knives that takes this path by exclusive create and unlinks it on drop
/// reads the persistent file as always held, and holds no OS lock while it
/// runs, so this lock acquires beside it and its unlink leaves the two
/// binaries on two inodes; the two must not run concurrently on one machine,
/// hooks included.
#[derive(Debug)]
pub(crate) struct FileLock {
    /// Dropping the handle releases the lock.
    _file: File,
}

impl FileLock {
    /// Beside the file it guards, named for that file's stem: `state.json` is
    /// guarded by `state.lock`. Waits per `wait`, then gives up loudly:
    /// blocking forever would hide a holder that has hung.
    pub(crate) fn acquire(target: &Path, wait: LockWait) -> Result<Self, LockError> {
        let path = target.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LockError::Io {
                path: path.clone(),
                source,
            })?;
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    let elapsed = started.elapsed();
                    if elapsed >= wait.deadline {
                        let holder = read_holder(&path).map_err(|source| LockError::Io {
                            path: path.clone(),
                            source,
                        })?;
                        return Err(LockError::Held { path, holder });
                    }
                    let remaining = wait.deadline.saturating_sub(elapsed);
                    std::thread::sleep(pause(attempt, wait, random_u64()).min(remaining));
                    attempt += 1;
                }
                Err(TryLockError::Error(source)) => {
                    return Err(LockError::Io { path, source });
                }
            }
        }
        // Ours now: name ourselves for whoever waits on us. A write failure is
        // not a lock failure, only a nameless holder in the give-up message.
        if file.set_len(0).is_ok() {
            let _ = writeln!(file, "{}", std::process::id());
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: LockWait = LockWait {
        deadline: Duration::from_millis(200),
        floor: Duration::from_millis(5),
        ceiling: Duration::from_millis(40),
    };

    #[test]
    fn a_second_acquirer_is_refused_and_told_the_holders_pid_and_age() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let first = FileLock::acquire(&path, QUICK).expect("first writer");

        let error = FileLock::acquire(&path, QUICK).expect_err("second writer");

        let LockError::Held { holder, .. } = &error else {
            panic!("expected a Held error, got {error:?}");
        };
        assert_eq!(holder.pid, Some(std::process::id()));
        assert!(holder.held_for < Duration::from_secs(5), "{holder:?}");
        let text = error.to_string();
        assert!(
            text.contains(&format!("pid {}", std::process::id())),
            "{text}"
        );
        assert!(text.contains("holding for"), "{text}");

        drop(first);
        assert!(
            FileLock::acquire(&path, QUICK).is_ok(),
            "the lock outlived its holder"
        );
    }

    #[test]
    fn a_holder_that_wrote_no_pid_reports_an_unknown_holder_and_the_locks_age() {
        // Given: something holds the OS lock on the file without naming itself.
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let nameless = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.with_extension("lock"))
            .expect("open lock file");
        nameless.try_lock().expect("hold the lock");

        let error = FileLock::acquire(&path, QUICK).expect_err("locked");

        let LockError::Held { holder, .. } = &error else {
            panic!("expected a Held error with an aged holder, got {error:?}");
        };
        assert_eq!(holder.pid, None);
        assert!(holder.held_for < Duration::from_secs(5), "{holder:?}");
        let text = error.to_string();
        assert!(
            text.contains(&format!(
                "holder unknown, lock written {}s ago",
                holder.held_for.as_secs()
            )),
            "{text}"
        );
    }

    #[test]
    fn a_waiting_acquirer_succeeds_once_the_holder_releases() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let held = FileLock::acquire(&path, QUICK).expect("first writer");
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        let started = std::time::Instant::now();
        let patient = LockWait {
            deadline: Duration::from_secs(2),
            ..QUICK
        };
        let _second = FileLock::acquire(&path, patient).expect("acquired after release");
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "{:?}",
            started.elapsed()
        );
        holder.join().expect("holder thread");
    }

    /// The lock file the child-process helper below takes, when run as a child.
    const HELPER_LOCK_TARGET: &str = "KNIVES_TEST_LOCK_TARGET";

    /// Not a test: the body of the child process in
    /// [`a_lock_held_by_a_killed_process_is_free_at_once`]. Takes the lock on
    /// the target the environment names, says so on stdout, and sleeps until
    /// killed.
    #[test]
    #[ignore = "a helper run as a child process by the killed-holder test"]
    fn helper_holds_the_lock_until_killed() {
        let Some(target) = std::env::var_os(HELPER_LOCK_TARGET) else {
            return;
        };
        let _lock = FileLock::acquire(Path::new(&target), QUICK).expect("child takes the lock");
        println!("HOLDING");
        std::io::stdout().flush().expect("flush");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_held_by_a_killed_process_is_free_at_once() {
        // Given: another process holds the lock and is killed with SIGKILL, so
        // nothing of its own runs at exit.
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "lock::tests::helper_holds_the_lock_until_killed",
            ])
            .env(HELPER_LOCK_TARGET, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the holder");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = std::io::BufReader::new(stdout).lines();
        let held = lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.trim() == "HOLDING");
        assert!(held, "the child never reported holding the lock");
        let blocked = FileLock::acquire(&path, QUICK);
        assert!(
            matches!(blocked, Err(LockError::Held { .. })),
            "the child's lock was not seen: {blocked:?}"
        );

        // When: the holder is killed.
        child.kill().expect("SIGKILL the holder");
        let _ = child.wait();

        // Then: the lock is free at once; no file to remove, no age to judge.
        let started = std::time::Instant::now();
        let _mine = FileLock::acquire(&path, QUICK).expect("acquired after the kill");
        assert!(
            started.elapsed() < QUICK.deadline / 2,
            "waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pauses_double_to_the_ceiling_and_never_drop_below_the_floor() {
        for attempt in 0..12 {
            for random in [0, 1, u64::MAX / 3, u64::MAX] {
                let pause = pause(attempt, QUICK, random);
                let cap = QUICK
                    .floor
                    .saturating_mul(1 << attempt.min(16))
                    .min(QUICK.ceiling);
                assert!(
                    pause >= QUICK.floor && pause <= cap,
                    "attempt {attempt}: {pause:?} not in [{:?}, {cap:?}]",
                    QUICK.floor
                );
            }
        }
        assert_eq!(pause(0, QUICK, 0), QUICK.floor);
        assert_eq!(pause(10, QUICK, u64::MAX), QUICK.ceiling);
    }

    #[test]
    fn the_wait_gives_up_by_its_deadline() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let _first = FileLock::acquire(&path, QUICK).expect("first writer");
        let started = std::time::Instant::now();
        let _ = FileLock::acquire(&path, QUICK).expect_err("locked");
        let waited = started.elapsed();
        assert!(
            waited >= QUICK.deadline && waited < QUICK.deadline * 3,
            "{waited:?}"
        );
    }
}
