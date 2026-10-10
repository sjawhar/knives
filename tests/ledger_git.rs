#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! Ledger entries carried between machines through real git repositories: a
//! bare remote both machines share, and a ledger root per machine that is the
//! working tree of a git repository nothing ever stages into or checks out.

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use knives::ids::UpstreamName;
use knives::ledger::{Entry, Kind, Ledger};
use knives::ledger_git::{self, GitError, MachineRef, Repository};

/// A fork's ledger directory is its upstream's `<owner>/<name>`.
const REPO: &str = "acme/a-repo";
/// Another fork of the same owner, so that carrying [`REPO`] alone must still
/// leave this one's directory out of every tree.
const OTHER: &str = "acme/other-repo";
/// The directory [`REPO`] and [`OTHER`] share.
const OWNER: &str = "acme";

/// A bare remote and a temporary directory to hold the machines that share it.
fn remote() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("lab directory");
    let remote = root.path().join("ledger.git");
    lab::git_bare_repository(&remote);
    (root, remote)
}

/// One machine's ledger root: a git repository whose `origin` is `remote`,
/// holding one directory per managed repo as the config home does.
fn machine(root: &Path, name: &str, remote: &Path) -> PathBuf {
    let directory = root.join(name).join("ledger");
    lab::git_repository(&directory, &[("origin", remote.to_str().expect("utf-8"))]);
    directory
}

/// The repository at `machine`'s own `.git`, carrying `forks`.
fn carrying(machine: &Path, forks: &[&str]) -> Repository {
    Repository::new(
        &machine.join(".git"),
        machine,
        forks.iter().map(|fork| UpstreamName::new(*fork)),
    )
    .expect("a repository over the machine's ledger root")
}

/// The repository at `machine`'s own `.git`, carrying [`REPO`].
fn repository(machine: &Path) -> Repository {
    carrying(machine, &[REPO])
}

fn ledger(machine: &Path) -> Ledger {
    ledger_of(machine, REPO)
}

fn ledger_of(machine: &Path, fork: &str) -> Ledger {
    Ledger::at(machine.join(fork))
}

/// Append `count` notes by `owner`, each with `padding` bytes after its text.
///
/// Every entry this process appends gets its own nanosecond, so two calls
/// never leave a pair of file names to the random suffix alone.
fn append(machine: &Path, owner: &str, count: usize, padding: usize) {
    append_padded(&ledger(machine), owner, count, padding);
}

/// `count` unpadded notes by `owner` in `fork`'s directory.
fn append_to(machine: &Path, fork: &str, owner: &str, count: usize) {
    append_padded(&ledger_of(machine, fork), owner, count, 0);
}

fn append_padded(ledger: &Ledger, owner: &str, count: usize, padding: usize) {
    static STAMP: AtomicUsize = AtomicUsize::new(0);
    for index in 0..count {
        let nanosecond = STAMP.fetch_add(1, Ordering::Relaxed);
        ledger
            .append(&Entry {
                ts: format!("2026-10-08T12:00:00.{nanosecond:09}Z"),
                owner: owner.to_owned(),
                email: None,
                subject: Some("feat/transport".to_owned()),
                kind: Kind::Note,
                disposition: None,
                statement: None,
                text: format!("{owner} learned thing {index}\n{}", "x".repeat(padding)),
                evidence: Vec::new(),
                anchor: None,
                pr: None,
                parents: Vec::new(),
            })
            .expect("append an entry");
    }
}

/// The entry file names in `machine`'s ledger.
fn file_names(machine: &Path) -> BTreeSet<String> {
    std::fs::read_dir(machine.join(REPO))
        .expect("read the ledger directory")
        .map(|dirent| {
            dirent
                .expect("ledger dirent")
                .file_name()
                .into_string()
                .expect("utf-8 file name")
        })
        .collect()
}

/// Every `refs/knives/` ref in `repository` as `<name> <commit>` lines.
fn knives_refs(repository: &Path) -> String {
    lab::git_output(
        repository,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/knives/",
        ],
    )
}

fn commit_and_push(machine: &Path, name: &str) -> knives::ids::CommitId {
    let repository = repository(machine);
    let commit = ledger_git::commit_new_entries(&repository, name)
        .expect("commit new entries")
        .expect("there were new entries");
    ledger_git::push(&repository, "origin", name).expect("push the machine's own ref");
    commit
}

/// Every path in every tree of every commit `reference` reaches in `git_dir`.
fn paths_ever_under(git_dir: &Path, reference: &str) -> BTreeSet<String> {
    let git_dir = git_dir.to_str().expect("utf-8");
    let commits = lab::git_output(
        Path::new("/"),
        ["--git-dir", git_dir, "rev-list", reference],
    );
    commits
        .lines()
        .flat_map(|commit| {
            lab::git_output(
                Path::new("/"),
                [
                    "--git-dir",
                    git_dir,
                    "ls-tree",
                    "-r",
                    "-t",
                    "--name-only",
                    commit,
                ],
            )
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn entries_committed_on_one_machine_materialise_on_another_and_parse() {
    // Given: two entries written on alpha, committed to its ref and pushed.
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    let beta = machine(root.path(), "beta", &remote);
    append(&alpha, "ses_alpha", 2, 0);
    let commit = commit_and_push(&alpha, "alpha");

    // When: beta fetches every machine's ref and materialises what it lacks.
    let found = ledger_git::fetch(&repository(&beta), "origin").expect("fetch");
    let done = ledger_git::materialise(&repository(&beta), &found).expect("materialise");

    // Then: beta has both entry files under alpha's names, and they parse to
    // exactly the entries alpha wrote.
    assert_eq!(
        found,
        [MachineRef {
            machine: "alpha".to_owned(),
            commit: commit.clone(),
        }]
    );
    assert_eq!(done.written, 2);
    assert!(done.skipped.is_empty(), "{done:?}");
    assert_eq!(file_names(&beta), file_names(&alpha));
    let entries = ledger(&beta).entries().expect("beta's ledger parses");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries, ledger(&alpha).entries().expect("alpha's ledger"));
    // No index was ever written on either side, so there was none to lock.
    assert!(!alpha.join(".git").join("index").exists());
    assert!(!beta.join(".git").join("index").exists());
    assert_eq!(knives_refs(&remote), format!("refs/knives/alpha {commit}"));
}

#[test]
fn two_machines_exchange_entries_and_neither_ref_is_rewritten() {
    // Given: alpha and beta each commit their own entries to their own ref.
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    let beta = machine(root.path(), "beta", &remote);
    append(&alpha, "ses_alpha", 2, 0);
    append(&beta, "ses_beta", 3, 0);
    let alpha_first = commit_and_push(&alpha, "alpha");
    let beta_first = commit_and_push(&beta, "beta");

    // When: each fetches and materialises the other's entries.
    for machine in [&alpha, &beta] {
        let found = ledger_git::fetch(&repository(machine), "origin").expect("fetch");
        assert_eq!(found.len(), 2, "both machines' refs are on the remote");
        ledger_git::materialise(&repository(machine), &found).expect("materialise");
    }

    // Then: both hold all five entries, and the peer's entries are not taken
    // for new ones of their own.
    assert_eq!(file_names(&alpha), file_names(&beta));
    assert_eq!(file_names(&alpha).len(), 5);
    assert_eq!(
        ledger(&alpha).entries().expect("alpha's ledger"),
        ledger(&beta).entries().expect("beta's ledger")
    );
    for (machine, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        assert_eq!(
            ledger_git::commit_new_entries(&repository(machine), name).expect("commit"),
            None,
            "{name} recommitted its peer's entries as its own"
        );
    }

    // When: alpha writes once more and pushes; beta fetches again.
    append(&alpha, "ses_alpha_later", 1, 0);
    let alpha_second = commit_and_push(&alpha, "alpha");
    let found =
        ledger_git::fetch(&repository(&beta), "origin").expect("a fetch with no forced update");
    assert_eq!(
        ledger_git::materialise(&repository(&beta), &found)
            .expect("materialise")
            .written,
        1
    );

    // Then: alpha's ref only moved forward, beta's never moved, and the
    // remote holds exactly what each machine committed.
    lab::git_output(
        &alpha,
        [
            "merge-base",
            "--is-ancestor",
            alpha_first.as_str(),
            alpha_second.as_str(),
        ],
    );
    assert_eq!(
        knives_refs(&remote),
        format!("refs/knives/alpha {alpha_second}\nrefs/knives/beta {beta_first}")
    );
    assert_eq!(
        knives_refs(&beta),
        format!("refs/knives/beta {beta_first}"),
        "a fetch never writes a machine's own ref"
    );
    assert_eq!(file_names(&beta).len(), 6);
}

#[test]
fn a_reader_never_sees_a_partial_entry_while_materialise_writes() {
    // Given: alpha committed entries and beta fetched them. The reader parses
    // files in name order and materialise writes them in that same order, so a
    // reader listing the directory mid-write reaches the file being written
    // last, usually after it is whole — except the first file, which it
    // reaches at once. That one is made large to hold the window open; the
    // rest are large enough that writing one is not instant.
    const ENTRIES: usize = 100;
    const READERS: usize = 4;
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    let beta = machine(root.path(), "beta", &remote);
    append(&alpha, "ses_alpha", 1, 16 << 20);
    append(&alpha, "ses_alpha", ENTRIES - 1, 64 << 10);
    commit_and_push(&alpha, "alpha");
    let written_by_alpha = ledger(&alpha).entries().expect("alpha's ledger");
    let found = ledger_git::fetch(&repository(&beta), "origin").expect("fetch");

    // When: readers loop over beta's ledger while materialise writes into it,
    // each read kept as the stamp and text length of every entry it parsed.
    let done = AtomicBool::new(false);
    let reader = ledger(&beta);
    let beta_repository = repository(&beta);
    let (written, reads) = std::thread::scope(|scope| {
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                scope.spawn(|| {
                    let mut reads = Vec::new();
                    while !done.load(Ordering::Acquire) {
                        reads.push(reader.entries().map(|entries| {
                            entries
                                .into_iter()
                                .map(|entry| (entry.ts, entry.text.len()))
                                .collect::<Vec<_>>()
                        }));
                    }
                    reads
                })
            })
            .collect();
        let written = ledger_git::materialise(&beta_repository, &found);
        done.store(true, Ordering::Release);
        let reads: Vec<_> = readers
            .into_iter()
            .flat_map(|reader| reader.join().expect("a reader thread"))
            .collect();
        (written, reads)
    });

    // Then: every read parsed, and every entry any read saw was whole: one of
    // alpha's stamps with its full text, not a prefix that happened to parse.
    assert_eq!(written.expect("materialise").written, ENTRIES);
    let failures: Vec<_> = reads
        .iter()
        .filter_map(|read| read.as_ref().err())
        .collect();
    assert!(failures.is_empty(), "a reader failed: {failures:?}");
    let whole: BTreeSet<(String, usize)> = written_by_alpha
        .iter()
        .map(|entry| (entry.ts.clone(), entry.text.len()))
        .collect();
    let reads: Vec<_> = reads.into_iter().flatten().collect();
    for read in &reads {
        for seen in read {
            assert!(
                whole.contains(seen),
                "a reader saw a partial entry: {seen:?}"
            );
        }
    }
    assert!(
        reads.iter().any(|read| (1..ENTRIES).contains(&read.len())),
        "no read landed mid-materialise, so the test exercised no concurrency: {} reads",
        reads.len()
    );
    assert_eq!(
        ledger(&beta).entries().expect("final read"),
        written_by_alpha
    );
}

#[test]
fn committing_with_nothing_new_returns_none_and_moves_no_ref() {
    // Given: a machine whose ledger is empty.
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);

    // When / Then: there is nothing to commit, and no ref appears.
    assert_eq!(
        ledger_git::commit_new_entries(&repository(&alpha), "alpha").expect("commit"),
        None
    );
    assert_eq!(knives_refs(&alpha), "");

    // Given: one entry, committed.
    append(&alpha, "ses_alpha", 1, 0);
    let commit = ledger_git::commit_new_entries(&repository(&alpha), "alpha")
        .expect("commit")
        .expect("one new entry");

    // When / Then: a second commit finds nothing new and leaves the ref alone.
    assert_eq!(
        ledger_git::commit_new_entries(&repository(&alpha), "alpha").expect("commit"),
        None
    );
    assert_eq!(knives_refs(&alpha), format!("refs/knives/alpha {commit}"));
}

#[test]
fn a_push_of_any_ref_but_the_machines_own_is_refused() {
    // Given: alpha pushed its ref, and beta fetched it without ever committing.
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    let beta = machine(root.path(), "beta", &remote);
    append(&alpha, "ses_alpha", 1, 0);
    let commit = commit_and_push(&alpha, "alpha");
    ledger_git::fetch(&repository(&beta), "origin").expect("fetch");

    // When / Then: beta pushing alpha's ref is refused, and so is committing
    // as alpha, which would start a second history under alpha's name; so is
    // any name that would spell some other ref or refspec.
    assert!(matches!(
        ledger_git::push(&repository(&beta), "origin", "alpha"),
        Err(GitError::NotOwn { .. })
    ));
    append(&beta, "ses_beta", 1, 0);
    assert!(matches!(
        ledger_git::commit_new_entries(&repository(&beta), "alpha"),
        Err(GitError::NotOwn { .. })
    ));
    assert_eq!(knives_refs(&beta), "");
    for name in [
        "*",
        "alpha:alpha",
        "beta/alpha",
        "../heads/main",
        "alpha.lock",
    ] {
        assert!(
            matches!(
                ledger_git::push(&repository(&beta), "origin", name),
                Err(GitError::Name { .. })
            ),
            "{name:?} was not refused"
        );
    }
    assert!(matches!(
        ledger_git::push(&repository(&beta), "--mirror", "beta"),
        Err(GitError::Name { .. })
    ));
    // A machine that never committed and that the remote has never seen has
    // nothing to send.
    ledger_git::push(&repository(&beta), "origin", "beta").expect("an empty push");
    assert_eq!(knives_refs(&remote), format!("refs/knives/alpha {commit}"));
}

/// A commit holding one blob at `name` under the root, built by hand in
/// `repository`: the shape a hostile or broken peer could push.
fn commit_with(repository: &Path, name: &str, mode: &str) -> knives::ids::CommitId {
    let blob = lab::git_output(repository, ["hash-object", "-w", "/dev/null"]);
    let script = format!("printf '{mode} blob {blob}\\t{name}\\0' | git mktree -z");
    let output = std::process::Command::new("sh")
        .args(["-c", &script])
        .current_dir(repository)
        .output()
        .expect("run mktree");
    assert!(output.status.success(), "mktree failed");
    let tree = String::from_utf8(output.stdout).expect("utf-8 tree id");
    knives::ids::CommitId::new(lab::git_output(
        repository,
        ["commit-tree", "--no-gpg-sign", "-m", "hostile", tree.trim()],
    ))
}

#[test]
fn a_peer_tree_that_is_not_ledger_entries_is_refused_and_nothing_is_written() {
    // Given: refs whose trees name a path outside the ledger, a `.git`, a
    // file that is not an entry, and an executable.
    let (root, remote) = remote();
    let beta = machine(root.path(), "beta", &remote);
    for (name, mode) in [
        ("..", "100644"),
        (".git", "100644"),
        (".GIT", "100644"),
        ("hooks.sh", "100644"),
        ("entry.md", "100755"),
        ("link.md", "120000"),
    ] {
        let hostile = MachineRef {
            machine: "mallory".to_owned(),
            commit: commit_with(&beta, name, mode),
        };

        // When / Then: materialise refuses it and writes nothing.
        assert!(
            matches!(
                ledger_git::materialise(&repository(&beta), &[hostile]),
                Err(GitError::NotAnEntry { .. })
            ),
            "{name:?} at {mode} was not refused"
        );
        let written: Vec<_> = std::fs::read_dir(&beta)
            .expect("read beta's ledger root")
            .map(|dirent| dirent.expect("dirent").file_name())
            .filter(|name| name != ".git")
            .collect();
        assert!(written.is_empty(), "{name:?} wrote {written:?}");
    }
}

#[test]
fn a_repository_commits_and_materialises_only_the_forks_it_carries() {
    // Given: alpha's ledger root holds entries for two forks, and the
    // repository over it carries only one of them.
    let (root, company) = remote();
    let alpha = machine(root.path(), "alpha", &company);
    append_to(&alpha, REPO, "ses_alpha", 2);
    append_to(&alpha, OTHER, "ses_alpha", 2);
    let carried = repository(&alpha);

    // When: it commits twice, the second time after both forks gained an
    // entry, and pushes.
    ledger_git::commit_new_entries(&carried, "alpha")
        .expect("commit")
        .expect("acme/a-repo's entries are new");
    append_to(&alpha, REPO, "ses_alpha", 1);
    append_to(&alpha, OTHER, "ses_alpha", 1);
    ledger_git::commit_new_entries(&carried, "alpha")
        .expect("commit")
        .expect("acme/a-repo's newest entry is new");
    ledger_git::push(&carried, "origin", "alpha").expect("push");

    // Then: no tree of any commit the ref reaches, here or on the remote,
    // names the fork it does not carry; every entry of the one it does is
    // there. The owner directory both forks share is in the tree, as the
    // parent of the one it carries.
    for git_dir in [alpha.join(".git"), company] {
        let paths = paths_ever_under(&git_dir, "refs/knives/alpha");
        assert!(
            paths
                .iter()
                .all(|path| path == OWNER || path == REPO || path.starts_with(&format!("{REPO}/"))),
            "{} carries a fork it was not given: {paths:?}",
            git_dir.display()
        );
        let carried_entries = paths.iter().filter(|path| is_entry(path)).count();
        assert_eq!(carried_entries, 3, "{paths:?}");
    }

    // Given: a peer that carries both forks pushed entries for both.
    let (peer_root, shared) = remote();
    let peer = machine(peer_root.path(), "peer", &shared);
    let beta = machine(peer_root.path(), "beta", &shared);
    append_to(&peer, REPO, "ses_peer", 1);
    append_to(&peer, OTHER, "ses_peer", 1);
    let both = carrying(&peer, &[REPO, OTHER]);
    ledger_git::commit_new_entries(&both, "peer")
        .expect("commit")
        .expect("both forks' entries are new");
    ledger_git::push(&both, "origin", "peer").expect("push");

    // When: a repository carrying only acme/a-repo fetches and materialises them.
    let only_repo = repository(&beta);
    let found = ledger_git::fetch(&only_repo, "origin").expect("fetch");
    let done = ledger_git::materialise(&only_repo, &found).expect("materialise");

    // Then: only acme/a-repo's entry is written; the other fork's directory
    // never appears, and its entry is counted as left unread.
    assert_eq!(done.written, 1);
    assert_eq!(
        done.skipped,
        std::collections::BTreeMap::from([(OTHER.to_owned(), 1)])
    );
    assert_eq!(file_names(&beta).len(), 1);
    assert!(!beta.join(OTHER).exists(), "an uncarried fork was written");
}

#[test]
fn two_repositories_over_one_ledger_root_each_carry_their_own_forks_to_their_own_remote() {
    // Given: one ledger root, its own `.git` carrying acme/a-repo to one
    // remote, and a git directory elsewhere over the same root carrying
    // acme/other-repo to another.
    let (root, company) = remote();
    let personal = root.path().join("personal-remote.git");
    lab::git_bare_repository(&personal);
    let alpha = machine(root.path(), "alpha", &company);
    let second_git_dir = root.path().join("second.git");
    lab::git_bare_repository(&second_git_dir);
    let second = second_git_dir.to_str().expect("utf-8");
    for args in [
        vec!["config", "core.bare", "false"],
        vec!["config", "core.worktree", alpha.to_str().expect("utf-8")],
        vec!["remote", "add", "origin", personal.to_str().expect("utf-8")],
    ] {
        let mut with_git_dir = vec!["--git-dir", second];
        with_git_dir.extend(args);
        let status = std::process::Command::new("git")
            .args(&with_git_dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {with_git_dir:?} failed");
    }
    append_to(&alpha, REPO, "ses_alpha", 2);
    append_to(&alpha, OTHER, "ses_alpha", 3);
    let first = repository(&alpha);
    let other = Repository::new(&second_git_dir, &alpha, [UpstreamName::new(OTHER)])
        .expect("a second repository over the root");

    // When: each commits as alpha and pushes to its own remote.
    for repository in [&first, &other] {
        ledger_git::commit_new_entries(repository, "alpha")
            .expect("commit")
            .expect("its fork's entries are new");
        ledger_git::push(repository, "origin", "alpha").expect("push");
    }

    // Then: each remote holds exactly its own fork's entries.
    for (remote, fork, count) in [(&company, REPO, 2), (&personal, OTHER, 3)] {
        let entries: Vec<_> = paths_ever_under(remote, "refs/knives/alpha")
            .into_iter()
            .filter(|path| is_entry(path))
            .collect();
        assert_eq!(entries.len(), count, "{}: {entries:?}", remote.display());
        assert!(
            entries
                .iter()
                .all(|path| path.starts_with(&format!("{fork}/"))),
            "{} holds another fork's entries: {entries:?}",
            remote.display()
        );
    }
    // And: the second repository's ref lives in its own git directory only.
    assert_eq!(
        paths_ever_under(&alpha.join(".git"), "refs/knives/alpha")
            .iter()
            .filter(|path| is_entry(path))
            .count(),
        2
    );
}

#[test]
fn a_fork_name_that_is_not_an_owner_and_a_name_is_refused() {
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    for name in [
        "",
        ".",
        "..",
        "a-repo",
        "../escape",
        "acme/..",
        "acme/a/b",
        "a\\b",
        "acme/a\\b",
        ".git",
        "acme/.GIT",
    ] {
        assert!(
            matches!(
                Repository::new(&alpha.join(".git"), &alpha, [UpstreamName::new(name)]),
                Err(GitError::Fork { .. })
            ),
            "{name:?} was accepted"
        );
    }
}

#[test]
fn a_repository_carrying_no_fork_commits_nothing() {
    let (root, remote) = remote();
    let alpha = machine(root.path(), "alpha", &remote);
    append(&alpha, "ses_alpha", 2, 0);
    let nothing = carrying(&alpha, &[]);

    assert_eq!(
        ledger_git::commit_new_entries(&nothing, "alpha").expect("commit"),
        None
    );
    assert!(!ledger_git::has_new_entries(&nothing, "alpha").expect("look"));
    assert_eq!(knives_refs(&alpha), "");
}

/// Whether `path` names a ledger entry file.
fn is_entry(path: &str) -> bool {
    Path::new(path).extension() == Some(std::ffi::OsStr::new("md"))
}
