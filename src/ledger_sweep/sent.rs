//! The send a write makes before it exits ([`send_now`]).
//!
//! An entry a command wrote about a fork that shares its ledger is on that
//! ledger's remote when the command exits, or the command says it is not
//! and exits 3: other machines decide from what the remote holds, and an
//! entry left here for some later command to carry is one they cannot see.
//! So the send runs in the command's own process once its work is done,
//! however that ended, with what it wrote ([`crate::ledger::appended`]). It
//! goes only through the destinations carrying an entry their remote lacks,
//! so a write costs one pass, as a sweep makes through that destination,
//! and a write whose entries a claim's push already carried costs none.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::destinations::ledger_of;
use super::join::joined_git_dir;
use super::kept::keep;
use super::{
    Destination, Destinations, REMOTE, Tally, destinations, ledger_repositories, pass, record,
};
use crate::config::Registry;
use crate::ids::UpstreamName;
use crate::ledger::{Appended, inline_human_text, problem_human_text};
use crate::ledger_git;
use crate::lock::{FileLock, LockWait};

/// A fork with entries this command wrote that its ledger's remote does not
/// hold once [`send_now`] is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsent {
    pub fork: UpstreamName,
    /// How many of the entries this command wrote about `fork` the remote
    /// lacks.
    pub entries: usize,
    /// The `<owner>/<name>` of the repository the fork's ledger belongs to.
    pub ledger: String,
    /// The git directory they were to go through; where knives looks for
    /// one, when no git directory could be read.
    pub git_dir: String,
    /// Why: git's own account, which quotes the remote, or what failed
    /// here. To show escaped.
    pub cause: String,
}

/// The line a command prints for an [`Unsent`]: what stayed on this
/// machine, where it was to go, why, and what sends it.
///
/// The fork and the ledger are registry values, and are not escaped; the
/// git directory is a path off disk, and the cause carries git's and the
/// remote's words, so both are.
impl fmt::Display for Unsent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} ledger entr{} this command wrote did not reach the ledger {} ({}): {}; \
             `knives ledger sweep` sends them",
            self.fork,
            self.entries,
            if self.entries == 1 { "y" } else { "ies" },
            self.ledger,
            inline_human_text(&self.git_dir),
            problem_human_text(&self.cause)
        )
    }
}

/// What [`send_now`] could not do.
#[derive(Debug, Default)]
pub struct SentNow {
    /// Each fork with an entry this command wrote that did not reach its
    /// ledger, in fork order.
    pub unsent: Vec<Unsent>,
    /// Each problem the send found that concerns no entry this command
    /// wrote: a fork it wrote nothing about whose ledger could not be
    /// joined, a directory of entries no fork is kept under, a peer's entry
    /// the pull did not write, a pass's failure that still sent every entry
    /// of this command's. Each is in the [`sweep_log`](super::sweep_log) too,
    /// as a sweep's problems are, but for a failure to write that log.
    pub problems: Vec<String>,
}

/// The entries this command wrote about one fork that shares its ledger.
#[derive(Debug)]
struct Shared<'a> {
    fork: &'a UpstreamName,
    /// The `<owner>/<name>` of the repository its ledger belongs to.
    ledger: &'a str,
    paths: Vec<&'a Path>,
}

/// [`Shared`] entries, as the destination carrying their fork reads them.
#[derive(Debug)]
struct Written<'a> {
    shared: &'a Shared<'a>,
    /// Each entry's file relative to the ledger root; `None` for one outside
    /// it, which no remote copy can be shown to hold.
    entries: Vec<Option<PathBuf>>,
}

impl Written<'_> {
    fn unsent(&self, entries: usize, through: &Destination, cause: String) -> Unsent {
        Unsent {
            fork: self.shared.fork.clone(),
            entries,
            ledger: self.shared.ledger.to_owned(),
            git_dir: through.repository.git_dir().display().to_string(),
            cause,
        }
    }
}

/// Send each of `appended` whose fork's `registry` entry names a ledger to
/// that ledger's remote, through the destinations of the ledger at `root`;
/// what did not reach it, and what else went wrong.
///
/// An entry of a fork with no `ledger` is not shared: nothing is read or
/// sent for it. A ledger that no git directory here reaches is joined first
/// ([`destinations`]). A destination whose remote copy already holds every
/// entry of `appended` it carries, as after a claim's push, is asked
/// nothing: when every one is held, nothing goes over the network and the
/// ledger's lock is not taken. Otherwise the lock is waited for as a sweep
/// waits for it, and each destination still lacking one of them, looked at
/// again under the lock since a command that held it meanwhile may have
/// carried them, gets one [`pass`] of its own: the pull, the discard of
/// each repeated transition, the commit and the push. An entry the pass
/// discarded as a repeat counts as sent, since a peer's ref carries it; one
/// this command removed again is not in `appended`
/// ([`crate::ledger::forget_appended`]). What the passes discarded and left
/// unread goes to the [`sweep_record`](super::sweep_record), and their
/// problems replace the [`sweep_log`](super::sweep_log), as a sweep's do.
pub fn send_now(root: &Path, registry: &Registry, appended: &[Appended]) -> SentNow {
    let shared = shared_of(registry, appended);
    if shared.is_empty() {
        return SentNow::default();
    }
    let reached = match destinations(root, registry) {
        Ok(reached) => reached,
        Err(error) => return unread(root, &shared, &error.to_string()),
    };
    let mut sent = SentNow::default();
    for each in &shared {
        if !reached
            .found
            .iter()
            .any(|destination| destination.carries(each.fork))
        {
            sent.unsent.push(Unsent {
                fork: each.fork.clone(),
                entries: each.paths.len(),
                ledger: each.ledger.to_owned(),
                git_dir: joined_git_dir(root, each.ledger).display().to_string(),
                cause: reached.uncarried_problem(root, each.fork, each.ledger),
            });
        }
    }
    let carried: Vec<Vec<Written<'_>>> = reached
        .found
        .iter()
        .map(|destination| written_through(destination, &shared))
        .collect();
    let lacking: Vec<Result<Vec<(&Written<'_>, usize)>, String>> = reached
        .found
        .iter()
        .zip(&carried)
        .map(|(destination, written)| lacked(destination, written))
        .collect();
    if lacking
        .iter()
        .all(|lacks| matches!(lacks.as_deref(), Ok([])))
    {
        sent.unsent.sort_by(|one, other| one.fork.cmp(&other.fork));
        return sent;
    }
    match FileLock::acquire(root, LockWait::TRANSPORT) {
        Ok(_lock) => {
            if let Some(passed) = pass_each(root, &reached, &carried, &mut sent) {
                let mut logged = Vec::new();
                for (fork, unreached) in &reached.unreached {
                    let problem = Destinations::unreached_problem(root, fork, unreached);
                    if !shared.iter().any(|each| each.fork == fork) {
                        sent.problems.push(problem.clone());
                    }
                    logged.push(problem);
                }
                sent.problems.extend(reached.unclaimed.iter().cloned());
                logged.extend(reached.unclaimed.iter().cloned());
                logged.extend(passed);
                if let Err(error) = record(root, &logged) {
                    sent.problems.push(error.to_string());
                }
            }
        }
        Err(error) => {
            let cause = error.to_string();
            for ((destination, written), lacks) in reached.found.iter().zip(&carried).zip(lacking) {
                let lacks = lacks.unwrap_or_else(|_| {
                    written
                        .iter()
                        .map(|written| (written, written.entries.len()))
                        .collect()
                });
                sent.unsent.extend(
                    lacks.into_iter().map(|(written, entries)| {
                        written.unsent(entries, destination, cause.clone())
                    }),
                );
            }
        }
    }
    sent.unsent.sort_by(|one, other| one.fork.cmp(&other.fork));
    sent
}

/// Each fork `appended` names whose `registry` entry names a ledger, in
/// fork order, with the entries of it.
fn shared_of<'a>(registry: &'a Registry, appended: &'a [Appended]) -> Vec<Shared<'a>> {
    let mut by_fork: BTreeMap<&UpstreamName, Vec<&Path>> = BTreeMap::new();
    for each in appended {
        by_fork.entry(&each.fork).or_default().push(&each.path);
    }
    by_fork
        .into_iter()
        .filter_map(|(fork, paths)| {
            Some(Shared {
                fork,
                ledger: ledger_of(registry, fork)?,
                paths,
            })
        })
        .collect()
}

/// What [`send_now`] says when where the ledger travels could not be read:
/// every entry of `shared` stayed here, for `cause`, with no git directory
/// to name but where knives looks for one.
fn unread(root: &Path, shared: &[Shared<'_>], cause: &str) -> SentNow {
    let anywhere = format!(
        "{} or one in {}",
        root.join(".git").display(),
        ledger_repositories(root).display()
    );
    SentNow {
        unsent: shared
            .iter()
            .map(|each| Unsent {
                fork: each.fork.clone(),
                entries: each.paths.len(),
                ledger: each.ledger.to_owned(),
                git_dir: anywhere.clone(),
                cause: cause.to_owned(),
            })
            .collect(),
        problems: Vec::new(),
    }
}

/// Each of `shared` whose fork `destination` carries, as it reads them.
fn written_through<'a>(destination: &Destination, shared: &'a [Shared<'a>]) -> Vec<Written<'a>> {
    let root = destination.repository.work_tree();
    shared
        .iter()
        .filter(|each| destination.carries(each.fork))
        .map(|each| Written {
            shared: each,
            entries: each
                .paths
                .iter()
                .map(|path| {
                    std::path::absolute(path)
                        .ok()?
                        .strip_prefix(root)
                        .ok()
                        .map(Path::to_path_buf)
                })
                .collect(),
        })
        .collect()
}

/// How many entries of each of `written` the remote copy through
/// `destination` lacks, for each it lacks any of; or why that could not be
/// read. The copy is what the last fetch or push through it saw, so this
/// asks the remote nothing.
fn lacked<'w, 'a>(
    destination: &Destination,
    written: &'w [Written<'a>],
) -> Result<Vec<(&'w Written<'a>, usize)>, String> {
    if written.is_empty() {
        return Ok(Vec::new());
    }
    let unsent = ledger_git::unsent(&destination.repository, REMOTE)
        .map_err(|error| format!("{}: {error}", destination.describe()))?;
    Ok(written
        .iter()
        .filter_map(|each| {
            let lacks = each
                .entries
                .iter()
                .filter(|entry| entry.as_ref().is_none_or(|path| unsent.contains(path)))
                .count();
            (lacks > 0).then_some((each, lacks))
        })
        .collect())
}

/// One [`pass`] through each destination of `reached` that still lacks an
/// entry of `carried`, under the ledger's lock, which the caller holds; each
/// entry still lacking goes to `sent` as unsent, and each problem
/// concerning none of them to its problems. Every problem of the passes is
/// returned, for the sweep log; `None` when no destination lacked one, so
/// that nothing passed and nothing is to be recorded.
fn pass_each(
    root: &Path,
    reached: &Destinations,
    carried: &[Vec<Written<'_>>],
    sent: &mut SentNow,
) -> Option<Vec<String>> {
    let mut logged = Vec::new();
    let mut passed = false;
    let mut tallies: Vec<Tally> = reached.found.iter().map(Tally::of).collect();
    for ((destination, written), tally) in reached.found.iter().zip(carried).zip(&mut tallies) {
        if matches!(lacked(destination, written).as_deref(), Ok([])) {
            continue;
        }
        passed = true;
        let mut failures = pass(destination, tally);
        match lacked(destination, written) {
            Ok(left) if left.is_empty() => sent.problems.extend(failures.iter().cloned()),
            Ok(left) => {
                let cause = cause_of(destination, &failures);
                sent.unsent.extend(
                    left.into_iter()
                        .map(|(each, entries)| each.unsent(entries, destination, cause.clone())),
                );
            }
            Err(error) => {
                failures.push(error);
                let cause = cause_of(destination, &failures);
                sent.unsent.extend(
                    written
                        .iter()
                        .map(|each| each.unsent(each.entries.len(), destination, cause.clone())),
                );
            }
        }
        logged.extend(failures);
    }
    if !passed {
        return None;
    }
    if let Err(error) = keep(root, &reached.found, &mut tallies) {
        sent.problems.push(error.to_string());
        logged.push(error.to_string());
    }
    for tally in &tallies {
        sent.problems.extend(tally.unwritten.iter().cloned());
        logged.extend(tally.unwritten.iter().cloned());
    }
    Some(logged)
}

/// Why a pass through `destination` left entries unsent: its `failures`,
/// each without the destination it names, which the line names already.
fn cause_of(destination: &Destination, failures: &[String]) -> String {
    if failures.is_empty() {
        return "no ref on the remote carries them after the push".to_owned();
    }
    let named = format!("{}: ", destination.describe());
    failures
        .iter()
        .map(|failure| failure.strip_prefix(named.as_str()).unwrap_or(failure))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;
    use crate::ids::BranchName;
    use crate::ledger::{Draft, Ledger, Scribe};
    use crate::ledger_git::{ClaimRecord, ClaimRef};
    use crate::ledger_sweep::kept::Kept;
    use crate::ledger_sweep::{ClaimChange, ClaimOutcome, change_claim, shared_fork, sweep_log};
    use crate::store::OwnerKind;

    const FORK: &str = "acme/a-repo";
    /// The ledger's URL as the git directory's `origin` spells it; git
    /// reaches it at the lab's bare remote through `insteadOf`.
    const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

    /// `git --git-dir <git_dir> <args>`, which must succeed; its stdout,
    /// trimmed.
    fn git(git_dir: &Path, args: &[&str]) -> String {
        let output = crate::bind::git_command()
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "lab")
            .env("GIT_AUTHOR_EMAIL", "lab@example.invalid")
            .env("GIT_COMMITTER_NAME", "lab")
            .env("GIT_COMMITTER_EMAIL", "lab@example.invalid")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn init(path: &Path, bare: bool) {
        let status = crate::bind::git_command()
            .args(["init", "--quiet"])
            .args(bare.then_some("--bare"))
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init {}", path.display());
    }

    /// A machine sharing [`FORK`]'s ledger with acme/ledger through a git
    /// directory in `ledger-repositories`, committing as `machine-one`, and
    /// the bare remote git reaches as [`LEDGER_URL`].
    struct Lab {
        _home: tempfile::TempDir,
        root: PathBuf,
        remote: PathBuf,
        git_dir: PathBuf,
        registry: Registry,
    }

    impl Lab {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join("ledger");
            let registry = home.path().join("repos.toml");
            std::fs::write(
                &registry,
                "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
                 origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"acme/ledger\"\n",
            )
            .unwrap();
            let registry = crate::config::load(&registry).unwrap();
            let remote = home.path().join("remote.git");
            init(&remote, true);
            let git_dir = ledger_repositories(&root).join("acme--ledger.git");
            init(&git_dir, true);
            let instead_of = format!("url.{}.insteadOf", remote.display());
            for args in [
                ["config", "core.bare", "false"].as_slice(),
                &["config", "core.worktree", root.to_str().unwrap()],
                &["remote", "add", "origin", LEDGER_URL],
                &["config", &instead_of, LEDGER_URL],
                &["config", "knives.machine", "machine-one"],
            ] {
                git(&git_dir, args);
            }
            Self {
                _home: home,
                root,
                remote,
                git_dir,
                registry,
            }
        }

        fn scribe(&self) -> Scribe {
            Scribe::unanchored(
                Ledger::at(self.root.join(FORK)),
                UpstreamName::new(FORK),
                "owner-one".to_owned(),
            )
        }

        /// What this process appended under this lab's root, as a command
        /// hands it to [`send_now`].
        fn appended(&self) -> Vec<Appended> {
            crate::ledger::appended()
                .into_iter()
                .filter(|appended| appended.path.starts_with(&self.root))
                .collect()
        }

        fn send(&self) -> SentNow {
            send_now(&self.root, &self.registry, &self.appended())
        }

        /// Route the remote's URL where nothing answers, so that any fetch
        /// or push through it fails.
        fn cut_off(&self) {
            git(
                &self.git_dir,
                &[
                    "config",
                    "--unset-all",
                    &format!("url.{}.insteadOf", self.remote.display()),
                ],
            );
            git(
                &self.git_dir,
                &[
                    "config",
                    "url./nonexistent/remote.git.insteadOf",
                    LEDGER_URL,
                ],
            );
        }

        /// Push to the remote, as `machine`'s ref, one commit carrying an
        /// entry reading `contents` at `relative` under the ledger root.
        fn push_as_peer(&self, machine: &str, relative: &str, contents: &str) {
            let scratch = tempfile::tempdir().unwrap();
            init(scratch.path(), false);
            let file = scratch.path().join(relative);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, contents).unwrap();
            let peer = scratch.path().join(".git");
            let work_tree = scratch.path().to_str().unwrap();
            git(&peer, &["--work-tree", work_tree, "add", "--all"]);
            git(
                &peer,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "--work-tree",
                    work_tree,
                    "commit",
                    "--quiet",
                    "-m",
                    "peer",
                ],
            );
            git(
                &peer,
                &[
                    "push",
                    "--quiet",
                    self.remote.to_str().unwrap(),
                    &format!("HEAD:refs/knives/{machine}"),
                ],
            );
        }
    }

    #[test]
    fn entries_a_claims_push_carried_are_sent_without_asking_the_remote() {
        // Given: a take of a branch whose push landed, carrying the entry
        // that records it, and then a remote nothing can reach.
        let lab = Lab::new();
        let fork = UpstreamName::new(FORK);
        let shared = shared_fork(&lab.root, &lab.registry, &fork)
            .unwrap()
            .unwrap();
        let claim = ClaimRef::new(&fork, &BranchName::new("feat/a")).unwrap();
        let scribe = lab.scribe();
        let changed = change_claim(
            ClaimChange {
                root: &lab.root,
                shared: &shared,
                claim: &claim,
                expected: None,
                becomes: Some(ClaimRecord {
                    repo: FORK.to_owned(),
                    branch: "feat/a".to_owned(),
                    owner: "owner-one".to_owned(),
                    kind: OwnerKind::HarnessSession,
                    why: "port the parser".to_owned(),
                    started: "2026-11-16T08:00:00Z".to_owned(),
                    place: None,
                    machine: "machine-one".to_owned(),
                    entry: String::new(),
                }),
            },
            || scribe.record_to(&Draft::event(Some("feat/a"), "claimed".into(), None)),
        );
        assert!(
            matches!(changed.outcome, ClaimOutcome::Landed { .. }),
            "{:?}",
            changed.outcome
        );
        lab.cut_off();

        // When: the command's entries are sent as it exits.
        let sent = lab.send();

        // Then: every one is sent, and nothing asked the remote, since a
        // fetch or a push through it would have failed.
        assert_eq!(sent.unsent, Vec::new());
        assert_eq!(sent.problems, Vec::<String>::new());
        assert!(!sweep_log(&lab.root).exists());
    }

    #[test]
    fn an_entry_discarded_as_a_repeat_is_sent_and_the_discard_is_kept() {
        // Given: another machine's `#7 merged` on the remote, which this
        // machine has not pulled, and this machine's own record of the same
        // merge, written since.
        let lab = Lab::new();
        lab.push_as_peer(
            "machine-two",
            &format!("{FORK}/20261011T120000.000000000Z-0000.md"),
            "+++\nts = \"2026-10-11T12:00:00Z\"\nowner = \"owner-two\"\n\
             subject = \"feat/a\"\nkind = \"event\"\npr = 7\n+++\n#7 merged\n",
        );
        let (_, mine) = lab
            .scribe()
            .record_to(&Draft::event(Some("feat/a"), "#7 merged".into(), Some(7)))
            .unwrap();

        // When: it is sent.
        let sent = lab.send();

        // Then: it counts as sent, the peer's ref carrying the merge, and
        // this machine's repeat is gone rather than pushed.
        assert_eq!(sent.unsent, Vec::new());
        assert_eq!(sent.problems, Vec::<String>::new());
        assert!(!mine.exists(), "{}", mine.display());
        assert_eq!(
            git(&lab.remote, &["for-each-ref", "refs/knives/machine-one"]),
            ""
        );
        // And: the sweep record keeps the discard, for `status` to say.
        let notes = Kept::read(&lab.root)
            .unwrap()
            .notes_for(&UpstreamName::new(FORK));
        assert!(
            notes
                .iter()
                .any(|note| note.contains("discarded 1 repeated transition")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_entry_no_remote_takes_stays_unsent_naming_the_ledger_and_the_git_directory() {
        // Given: this machine's entry, and a remote nothing can reach.
        let lab = Lab::new();
        lab.cut_off();
        let (_, mine) = lab
            .scribe()
            .record_to(&Draft::event(Some("feat/a"), "noted".into(), None))
            .unwrap();

        // When: it is sent.
        let sent = lab.send();

        // Then: it stayed here, named with its fork, its ledger and the git
        // directory it was to go through, with git's account of the push
        // and not the destination again; the failure is in the sweep log
        // too.
        let [unsent] = sent.unsent.as_slice() else {
            panic!("{:?}", sent.unsent);
        };
        assert_eq!(unsent.fork, UpstreamName::new(FORK));
        assert_eq!(unsent.entries, 1);
        assert_eq!(unsent.ledger, "acme/ledger");
        assert_eq!(unsent.git_dir, lab.git_dir.display().to_string());
        assert!(
            unsent.cause.contains("/nonexistent/remote.git")
                && !unsent.cause.starts_with("origin ("),
            "{}",
            unsent.cause
        );
        assert!(mine.is_file());
        assert_eq!(sent.problems, Vec::<String>::new());
        let log = std::fs::read_to_string(sweep_log(&lab.root)).unwrap();
        assert!(log.contains("/nonexistent/remote.git"), "{log}");
    }
}
