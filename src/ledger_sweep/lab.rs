//! What the tests of a claim's change ([`super::change_claim`]) and of a
//! write's send ([`super::send_now`]) both stand on: a machine sharing a
//! fork's ledger through a git directory in [`ledger_repositories`], as
//! one set up by hand to share it has, and the bare remote git reaches as
//! that ledger's repository. The tests of a join use its git helpers too.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::ledger_repositories;
use crate::config::Registry;
use crate::ids::CommitId;

/// The fork whose ledger the lab shares.
pub(super) const FORK: &str = "acme/a-repo";
/// The ledger's URL as the git directory's `origin` spells it; git
/// reaches it at the lab's bare remote through `insteadOf`.
pub(super) const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

/// `git --git-dir <git_dir> <args>`, which must succeed; its stdout,
/// trimmed.
pub(super) fn git(git_dir: &Path, args: &[&str]) -> String {
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

/// `git init` at `path`, bare when `bare`, which must succeed.
pub(super) fn init(path: &Path, bare: bool) {
    let status = crate::bind::git_command()
        .args(["init", "--quiet"])
        .args(bare.then_some("--bare"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "git init {}", path.display());
}

/// The commit `reference` holds in `git_dir`, if it exists.
pub(super) fn at(git_dir: &Path, reference: &str) -> Option<CommitId> {
    let id = git(
        git_dir,
        &["for-each-ref", "--format=%(objectname)", reference],
    );
    (!id.is_empty()).then(|| CommitId::new(id))
}

/// A machine sharing [`FORK`]'s ledger with acme/ledger through a git
/// directory in `ledger-repositories`, committing as `machine-one`, and
/// the bare remote git reaches as [`LEDGER_URL`].
#[derive(Debug)]
pub(super) struct Lab {
    pub(super) home: tempfile::TempDir,
    pub(super) root: PathBuf,
    pub(super) remote: PathBuf,
    pub(super) git_dir: PathBuf,
    pub(super) registry: Registry,
}

impl Lab {
    pub(super) fn new() -> Self {
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
        let work_tree = root.to_str().unwrap();
        let instead_of = format!("url.{}.insteadOf", remote.display());
        for args in [
            ["config", "core.bare", "false"].as_slice(),
            &["config", "core.worktree", work_tree],
            &["remote", "add", "origin", LEDGER_URL],
            &["config", &instead_of, LEDGER_URL],
            &["config", "knives.machine", "machine-one"],
        ] {
            git(&git_dir, args);
        }
        Self {
            home,
            root,
            remote,
            git_dir,
            registry,
        }
    }
}
