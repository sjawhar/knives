#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]
#![allow(
    clippy::unused_self,
    unreachable_pub,
    dead_code,
    reason = "a test fixture included by path: method form keeps call sites readable, and not every test target uses every helper"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The environment every git, jj and knives a test starts runs in, set once for
/// the whole process before `main`.
#[path = "isolation.rs"]
mod isolation;

pub struct Lab {
    temp: TempDir,
    trunk: String,
    pub(crate) upstream: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) second: PathBuf,
    pub(crate) maintainer: PathBuf,
}

impl Lab {
    pub(crate) fn new() -> Self {
        Self::with_trunk("main")
    }

    pub(crate) fn with_trunk(trunk: &str) -> Self {
        Self::build(TempDir::new().expect("create lab directory"), trunk)
    }

    /// [`Self::new`] in a directory under `parent`, so that one scan root
    /// (`HOME`) holds the checkouts of several labs, each its own fork.
    pub(crate) fn new_in(parent: &Path) -> Self {
        Self::build(
            TempDir::new_in(parent).expect("create lab directory"),
            "main",
        )
    }

    fn build(temp: TempDir, trunk: &str) -> Self {
        let upstream = temp.path().join("upstream.git");
        let origin = temp.path().join("origin.git");
        let seed = temp.path().join("seed");
        let maintainer = temp.path().join("maintainer");
        let work = temp.path().join("work");
        let second = temp.path().join("second");

        git(
            temp.path(),
            trunk,
            ["init", "--bare", upstream.to_str().expect("utf-8 path")],
        );
        git(
            temp.path(),
            trunk,
            ["init", "--bare", origin.to_str().expect("utf-8 path")],
        );
        git(
            temp.path(),
            trunk,
            [
                "clone",
                upstream.to_str().expect("utf-8 path"),
                seed.to_str().expect("utf-8 path"),
            ],
        );
        std::fs::write(seed.join("README.md"), "seed\n").expect("write seed");
        git(&seed, trunk, ["add", "README.md"]);
        git(&seed, trunk, ["commit", "-m", "seed"]);
        git(&seed, trunk, ["branch", "-M", trunk]);
        git(&seed, trunk, ["push", "origin", trunk]);
        git(
            &seed,
            trunk,
            [
                "remote",
                "add",
                "fork",
                origin.to_str().expect("utf-8 path"),
            ],
        );
        git(&seed, trunk, ["push", "fork", trunk]);
        git(
            temp.path(),
            trunk,
            [
                "clone",
                upstream.to_str().expect("utf-8 path"),
                maintainer.to_str().expect("utf-8 path"),
            ],
        );
        jj(
            temp.path(),
            [
                "git",
                "clone",
                origin.to_str().expect("utf-8 path"),
                work.to_str().expect("utf-8 path"),
            ],
        );
        configure_jj_repo_identity(&work);
        jj(
            &work,
            [
                "git",
                "remote",
                "add",
                "upstream",
                upstream.to_str().expect("utf-8 path"),
            ],
        );
        jj(&work, ["git", "fetch", "--remote", "upstream"]);
        jj(
            temp.path(),
            [
                "git",
                "clone",
                origin.to_str().expect("utf-8 path"),
                second.to_str().expect("utf-8 path"),
            ],
        );
        configure_jj_repo_identity(&second);

        Self {
            temp,
            trunk: trunk.to_owned(),
            upstream,
            work,
            second,
            maintainer,
        }
    }

    pub(crate) fn branch(&self, name: &str, file: &str, content: &str) {
        self.branch_with_bytes(name, file, content.as_bytes());
    }

    /// [`Self::branch`] with content that need not be UTF-8.
    pub(crate) fn branch_with_bytes(&self, name: &str, file: &str, content: &[u8]) {
        let origin_trunk = format!("{}@origin", self.trunk);
        jj(&self.work, ["new", "-r", &origin_trunk, "-m", name]);
        std::fs::write(self.work.join(file), content).expect("write branch content");
        jj(&self.work, ["bookmark", "create", name, "-r", "@"]);
        jj(&self.work, ["new"]);
    }

    pub(crate) fn push_branch(&self, name: &str) {
        jj(
            &self.work,
            ["git", "push", "--remote", "origin", "--bookmark", name],
        );
    }

    pub(crate) fn advance_origin_branch(&self, name: &str, content: &str) {
        jj(&self.second, ["git", "fetch", "--remote", "origin"]);
        jj(
            &self.second,
            ["bookmark", "track", name, "--remote", "origin"],
        );
        let remote_branch = format!("{name}@origin");
        jj(
            &self.second,
            ["new", "-r", &remote_branch, "-m", "origin advance"],
        );
        std::fs::write(self.second.join("origin-advance.txt"), content)
            .expect("write origin advance");
        jj(&self.second, ["bookmark", "set", name, "-r", "@"]);
        jj(&self.second, ["new"]);
        jj(
            &self.second,
            ["git", "push", "--remote", "origin", "--bookmark", name],
        );
    }

    /// A branch some other clone pushed to origin, forked from `base` — a revset
    /// in the second clone, such as `main@origin` or `feat/ours@origin` — and
    /// touching `<name>.txt`. After the work checkout fetches, it is
    /// `<name>@origin` there: an untracked remote bookmark, which jj's default
    /// `immutable_heads()` pins along with every commit beneath it.
    pub(crate) fn foreign_origin_branch(&self, base: &str, name: &str, content: &str) {
        jj(&self.second, ["git", "fetch", "--remote", "origin"]);
        jj(&self.second, ["new", "-r", base, "-m", name]);
        std::fs::write(self.second.join(format!("{name}.txt")), content)
            .expect("write foreign branch");
        jj(&self.second, ["bookmark", "create", name, "-r", "@"]);
        jj(&self.second, ["new"]);
        jj(
            &self.second,
            ["git", "push", "--remote", "origin", "--bookmark", name],
        );
    }

    pub(crate) fn publish_pull(&self, branch: &str, number: u64) {
        let destination = format!("refs/heads/{branch}:refs/pull/{number}/head");
        git(&self.work, &self.trunk, ["push", "upstream", &destination]);
    }

    pub(crate) fn squash_merge_pull(&self, number: u64, changed_content: Option<&str>) {
        let pull = format!("refs/pull/{number}/head:refs/remotes/origin/pull/{number}/head");
        git(&self.maintainer, &self.trunk, ["fetch", "origin", &pull]);
        git(
            &self.maintainer,
            &self.trunk,
            ["checkout", self.trunk.as_str()],
        );
        let pull = format!("origin/pull/{number}/head");
        git(&self.maintainer, &self.trunk, ["merge", "--squash", &pull]);
        if let Some(content) = changed_content {
            std::fs::write(self.maintainer.join("feature.txt"), content)
                .expect("alter squashed content");
            git(&self.maintainer, &self.trunk, ["add", "feature.txt"]);
        }
        git(
            &self.maintainer,
            &self.trunk,
            ["commit", "-m", "squash merge"],
        );
        git(
            &self.maintainer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// Merge a published pull into upstream's trunk with a real merge commit,
    /// the way a merge-commit forge button does: the branch tip itself becomes
    /// an ancestor of the trunk.
    pub(crate) fn merge_pull_with_merge_commit(&self, number: u64) {
        self.merge_pull_with_merge_commit_unfetched(number);
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// [`Self::merge_pull_with_merge_commit`] without the work checkout
    /// fetching upstream afterwards: its `main@upstream` view stays behind.
    pub(crate) fn merge_pull_with_merge_commit_unfetched(&self, number: u64) {
        let pull = format!("refs/pull/{number}/head:refs/remotes/origin/pull/{number}/head");
        git(&self.maintainer, &self.trunk, ["fetch", "origin", &pull]);
        git(
            &self.maintainer,
            &self.trunk,
            ["checkout", self.trunk.as_str()],
        );
        let pull = format!("origin/pull/{number}/head");
        git(
            &self.maintainer,
            &self.trunk,
            ["merge", "--no-ff", "-m", "merge pull", &pull],
        );
        git(
            &self.maintainer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
    }

    /// Fast-forward upstream's trunk to a published pull, as a fast-forward
    /// merge or a direct push lands it: the branch's commits become the
    /// trunk's own first-parent line, with no merge commit.
    pub(crate) fn fast_forward_pull(&self, number: u64) {
        let pull = format!("refs/pull/{number}/head:refs/remotes/origin/pull/{number}/head");
        git(&self.maintainer, &self.trunk, ["fetch", "origin", &pull]);
        git(
            &self.maintainer,
            &self.trunk,
            ["checkout", self.trunk.as_str()],
        );
        let pull = format!("origin/pull/{number}/head");
        git(&self.maintainer, &self.trunk, ["merge", "--ff-only", &pull]);
        git(
            &self.maintainer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// Bring the fork's trunk up to upstream's and fetch it into the work
    /// checkout, so `main@origin` is ahead of a `main@upstream` view nobody
    /// refreshed.
    pub(crate) fn mirror_upstream_trunk_to_origin(&self) {
        let refspec = format!("{0}:{0}", self.trunk);
        git(
            &self.maintainer,
            &self.trunk,
            [
                "push",
                self.temp_origin().to_str().expect("utf-8 path"),
                &refspec,
            ],
        );
        jj(&self.work, ["git", "fetch", "--remote", "origin"]);
    }

    pub(crate) fn rebase_and_force_push(&self, branch: &str) {
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
        let upstream_trunk = format!("{}@upstream", self.trunk);
        jj(&self.work, ["rebase", "-b", branch, "-o", &upstream_trunk]);
        jj(
            &self.work,
            ["git", "push", "--remote", "origin", "--bookmark", branch],
        );
    }

    pub(crate) fn advance_upstream(&self, content: &str) {
        self.advance_upstream_unfetched(content);
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// Commit `file` on upstream's trunk (creating its directories) and fetch it into
    /// the work checkout, so `<trunk>@upstream` carries it.
    pub(crate) fn upstream_trunk_file(&self, file: &str, content: &str) {
        let path = self.maintainer.join(file);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dirs");
        std::fs::write(&path, content).expect("write upstream trunk file");
        git(&self.maintainer, &self.trunk, ["add", file]);
        git(
            &self.maintainer,
            &self.trunk,
            ["commit", "-m", "upstream trunk file"],
        );
        git(
            &self.maintainer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// A branch of upstream's own, forked from its trunk tip and pushed there:
    /// somebody else's work, visible here as `<name>@upstream` after the fetch.
    pub(crate) fn upstream_branch(&self, name: &str, file: &str, content: &str) {
        git(
            &self.maintainer,
            &self.trunk,
            ["checkout", "-b", name, self.trunk.as_str()],
        );
        std::fs::write(self.maintainer.join(file), content).expect("write upstream branch");
        git(&self.maintainer, &self.trunk, ["add", file]);
        git(&self.maintainer, &self.trunk, ["commit", "-m", name]);
        git(&self.maintainer, &self.trunk, ["push", "origin", name]);
        git(
            &self.maintainer,
            &self.trunk,
            ["checkout", self.trunk.as_str()],
        );
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// An annotated tag of upstream's own on `revision` in its repository, pushed
    /// there and fetched: `<name>` and `<name>@upstream` here.
    pub(crate) fn upstream_tag(&self, name: &str, revision: &str) {
        git(
            &self.maintainer,
            &self.trunk,
            [
                "-c",
                "tag.gpgSign=false",
                "tag",
                "-a",
                "-m",
                name,
                name,
                revision,
            ],
        );
        git(&self.maintainer, &self.trunk, ["push", "origin", name]);
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    /// [`Self::advance_upstream`] without the work checkout fetching upstream
    /// afterwards: its `main@upstream` view stays behind.
    pub(crate) fn advance_upstream_unfetched(&self, content: &str) {
        std::fs::write(self.maintainer.join("UPSTREAM.md"), content)
            .expect("write upstream advance");
        git(&self.maintainer, &self.trunk, ["add", "UPSTREAM.md"]);
        git(
            &self.maintainer,
            &self.trunk,
            ["commit", "-m", "advance upstream"],
        );
        git(
            &self.maintainer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
    }

    pub(crate) fn consumer_with_pin_history(
        &self,
        file: &str,
        checkout_content: &str,
        origin_content: &str,
    ) -> PathBuf {
        let origin = self.temp.path().join("consumer-origin.git");
        let seed = self.temp.path().join("consumer-seed");
        let consumer = self.temp.path().join("consumer");
        git(
            self.temp.path(),
            &self.trunk,
            ["init", "--bare", origin.to_str().expect("utf-8 path")],
        );
        git(
            self.temp.path(),
            &self.trunk,
            [
                "clone",
                origin.to_str().expect("utf-8 path"),
                seed.to_str().expect("utf-8 path"),
            ],
        );
        std::fs::write(seed.join("README.md"), "seed\n").expect("write consumer seed");
        git(&seed, &self.trunk, ["add", "README.md"]);
        git(&seed, &self.trunk, ["commit", "-m", "seed consumer"]);
        git(&seed, &self.trunk, ["push", "origin", self.trunk.as_str()]);
        git(
            self.temp.path(),
            &self.trunk,
            [
                "clone",
                origin.to_str().expect("utf-8 path"),
                consumer.to_str().expect("utf-8 path"),
            ],
        );

        std::fs::write(consumer.join(file), checkout_content).expect("write checkout pin");
        git(&consumer, &self.trunk, ["add", file]);
        git(&consumer, &self.trunk, ["commit", "-m", "old pin"]);
        git(
            &consumer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );

        std::fs::write(consumer.join(file), origin_content).expect("write origin pin");
        git(&consumer, &self.trunk, ["add", file]);
        git(&consumer, &self.trunk, ["commit", "-m", "new pin"]);
        git(
            &consumer,
            &self.trunk,
            ["push", "origin", self.trunk.as_str()],
        );
        git(&consumer, &self.trunk, ["reset", "--hard", "HEAD~1"]);
        git(&consumer, &self.trunk, ["fetch"]);

        consumer
    }

    pub(crate) fn octopus(&self, name: &str, first: &str, second: &str) {
        let origin_trunk = format!("{}@origin", self.trunk);
        jj(
            &self.work,
            [
                "new",
                "-r",
                &origin_trunk,
                "-r",
                first,
                "-r",
                second,
                "-m",
                name,
            ],
        );
        jj(&self.work, ["bookmark", "create", name, "-r", "@"]);
        jj(&self.work, ["new"]);
    }

    pub(crate) fn rewrite_local_branch(&self, branch: &str, content: &str) {
        jj(&self.work, ["edit", branch]);
        std::fs::write(self.work.join("feature.txt"), content).expect("rewrite branch");
        jj(&self.work, ["bookmark", "set", branch, "-r", "@"]);
        jj(&self.work, ["new"]);
    }

    /// Push `branch` from the work checkout and track it in the second clone,
    /// so both clones can rewrite it.
    pub(crate) fn track_in_second_clone(&self, branch: &str) {
        self.push_branch(branch);
        jj(&self.second, ["git", "fetch", "--remote", "origin"]);
        let remote_branch = format!("{branch}@origin");
        jj(&self.second, ["bookmark", "track", &remote_branch]);
    }

    /// Rewrite `branch` in the second clone and push it: the shape another
    /// agent's force-push leaves, which the next fetch in the work checkout
    /// turns into a divergent bookmark when the work checkout rewrote it too.
    pub(crate) fn rewrite_in_second_clone(&self, branch: &str, content: &str) {
        jj(&self.second, ["edit", "--ignore-immutable", branch]);
        std::fs::write(self.second.join("feature.txt"), content).expect("rewrite second clone");
        jj(&self.second, ["bookmark", "set", branch, "-r", "@"]);
        jj(&self.second, ["new"]);
        jj(
            &self.second,
            ["git", "push", "--remote", "origin", "--bookmark", branch],
        );
    }

    pub(crate) fn rewrite_in_both_clones(&self, branch: &str) {
        self.track_in_second_clone(branch);
        self.rewrite_local_branch(branch, "work rewrite\n");
        self.rewrite_in_second_clone(branch, "second rewrite\n");
        jj(&self.work, ["git", "fetch", "--remote", "origin"]);
    }

    pub(crate) fn revision(&self, directory: &Path, revision: &str, template: &str) -> String {
        jj_output(
            directory,
            [
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                revision,
                "-T",
                template,
            ],
        )
    }

    pub(crate) fn work_path(&self) -> &Path {
        &self.work
    }

    pub fn temp_path(&self) -> &Path {
        self.temp.path()
    }

    pub(crate) fn temp_origin(&self) -> PathBuf {
        self.temp.path().join("origin.git")
    }

    pub(crate) fn repo_entry_with_release_branch(&self, name: &str) -> knives::config::RepoEntry {
        knives::config::RepoEntry {
            release_branch: Some(name.to_owned()),
            // The work directory deliberately stands in for origin, per lab convention.
            ..knives::config::RepoEntry::new(
                self.upstream.display().to_string(),
                self.work.display().to_string(),
            )
        }
    }

    /// Point the work checkout's `upstream` remote at the forge URL `url`,
    /// which git rewrites to the lab's upstream repository when it fetches.
    ///
    /// Every other lab fork's upstream is a filesystem path, which names no
    /// forge repository, so its claims, sightings and ledger are kept under
    /// its registry key and a reader keyed by that key looks right. A fork
    /// whose upstream is a forge URL, as every real one is, is kept under
    /// the URL's lowercase `<owner>/<name>` instead.
    pub(crate) fn upstream_at_forge_url(&self, url: &str) {
        let rewrite = format!("url.{}.insteadOf", self.upstream.display());
        git(&self.work, &self.trunk, ["config", &rewrite, url]);
        git(
            &self.work,
            &self.trunk,
            ["remote", "set-url", "upstream", url],
        );
    }

    /// Point the work checkout's `origin` remote at the forge URL `url`,
    /// which git rewrites to the lab's origin repository when it fetches or
    /// pushes: the spelling a real fork's `origin` remote and its registry
    /// entry share.
    pub(crate) fn origin_at_forge_url(&self, url: &str) {
        let rewrite = format!("url.{}.insteadOf", self.temp_origin().display());
        git(&self.work, &self.trunk, ["config", &rewrite, url]);
        git(
            &self.work,
            &self.trunk,
            ["remote", "set-url", "origin", url],
        );
    }
}
/// A registry entry for the lab's work checkout, which stands in for origin.
pub fn lab_entry(lab: &Lab) -> knives::config::RepoEntry {
    knives::config::RepoEntry::new(
        lab.upstream.display().to_string(),
        lab.work.display().to_string(),
    )
}

/// The lab's work checkout bound to `entry` under the registry name `name`, with
/// the remotes the checkout really declares.
pub fn lab_fork<'a>(
    lab: &Lab,
    name: &str,
    entry: &'a knives::config::RepoEntry,
) -> knives::bind::Fork<'a> {
    knives::bind::Fork {
        name: knives::ids::RepoName::new(name),
        upstream: entry.upstream_name(name),
        entry,
        checkout: knives::bind::Checkout {
            path: lab.work.clone(),
            remotes: knives::bind::remotes(&lab.work).expect("read the work checkout's remotes"),
        },
    }
}
/// Registry home plus a local consumer for release tests. The registry deliberately
/// keeps no local consumer path: command helpers supply this checkout via
/// `--consumer`, as production callers must.
pub fn release_test_home(lab: &Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    release_test_home_pinned(
        lab,
        "branch = \"release/2026-08-03\"",
        "branch = \"release/2026-08-04\"",
    )
}

/// [`release_test_home`] with the consumer's checkout and origin pins spelled by
/// the caller: `branch = "…"` follows a release, `rev = "…"` freezes on one.
pub fn release_test_home_pinned(
    lab: &Lab,
    checkout_pin: &str,
    origin_pin: &str,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        &format!("work = {{ git = \"https://forge.invalid/acme/work.git\", {checkout_pin} }}\n"),
        &format!("work = {{ git = \"https://forge.invalid/acme/work.git\", {origin_pin} }}\n"),
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write registry");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.display().to_string(),
    )
    .expect("write local consumer fixture path");
    (home, consumer)
}
/// The commit `revision` resolves to in the work checkout right now.
pub fn commit_at(lab: &Lab, revision: &str) -> knives::ids::CommitId {
    knives::jj::Repo::open(&lab.work)
        .expect("open to resolve a revision")
        .resolve_commit(revision)
        .expect("resolve revision")
}

/// The commits a release named `name` has as parents right now.
pub fn release_parents(lab: &Lab, name: &str) -> Vec<knives::ids::CommitId> {
    knives::jj::Repo::open(&lab.work)
        .expect("open for release parents")
        .parents_of(name)
        .expect("read release parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect()
}

/// Operation ids in the shared op log, newest first.
pub fn operation_ids(repo: &std::path::Path) -> Vec<String> {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ])
        .current_dir(repo)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read op log");
    assert!(output.status.success(), "read op log");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
#[derive(Clone, Copy)]
pub enum ReleaseOutput {
    Text,
    Json,
}

impl ReleaseOutput {
    const fn flag(self) -> &'static str {
        match self {
            Self::Text => "--text",
            Self::Json => "--json",
        }
    }
}

pub fn release_command(
    lab: &Lab,
    home: &tempfile::TempDir,
    output: ReleaseOutput,
    args: &[&str],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args([output.flag(), "release", "--repo", "demo"]);
    let local_consumer = home.path().join("local-consumer");
    if local_consumer.exists() {
        let consumer =
            std::fs::read_to_string(&local_consumer).expect("read local consumer fixture path");
        command.args(["--consumer", consumer.trim()]);
    }
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path());
    command
}

/// `knives start <branch> --repo demo --why test` from the work checkout, for
/// callers that add an environment variable before running it.
pub fn start_command(lab: &Lab, home: &tempfile::TempDir, branch: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(["--text", "start", branch, "--repo", "demo", "--why", "test"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path());
    command
}

/// Run `knives start <branch> --repo demo --why test` from the work checkout.
pub fn knives_start(lab: &Lab, home: &tempfile::TempDir, branch: &str) -> std::process::Output {
    start_command(lab, home, branch)
        .output()
        .expect("run knives start")
}

/// Run the knives binary's release command for the `demo` repo in `lab`.
pub fn knives_release(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    release_command(lab, home, ReleaseOutput::Text, args)
        .output()
        .expect("run knives release")
}

fn git<const N: usize>(directory: &Path, trunk: &str, args: [&str; N]) {
    let status = git_command(directory, trunk)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git command failed");
}

/// `git` in `directory` with the lab's identity and initial branch pinned.
fn git_command(directory: &Path, trunk: &str) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Knives Lab")
        .env("GIT_AUTHOR_EMAIL", "knives-lab@example.test")
        .env("GIT_COMMITTER_NAME", "Knives Lab")
        .env("GIT_COMMITTER_EMAIL", "knives-lab@example.test")
        // Pin the initial branch name instead of inheriting `init.defaultBranch` from
        // whoever is running the tests. Without this the lab's branch varies by machine,
        // so its configured push can fail with "src refspec does not match any" — green
        // locally, red in CI, which is exactly the difference a test harness must avoid.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_0", trunk);
    command
}

fn configure_jj_repo_identity(directory: &Path) {
    // Pin identity in each jj fixture repository instead of only on the lab's
    // commands. Knives spawns jj itself, so a clean machine with no ambient
    // Git identity otherwise creates authorless commits that cannot be pushed
    // — green locally, red in CI, exactly the difference this harness must avoid.
    jj(
        directory,
        ["config", "set", "--repo", "user.name", "Knives Lab"],
    );
    jj(
        directory,
        [
            "config",
            "set",
            "--repo",
            "user.email",
            "knives-lab@example.test",
        ],
    );
}

pub fn jj<const N: usize>(directory: &Path, args: [&str; N]) {
    let status = Command::new("jj")
        .args(args)
        .current_dir(directory)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("run jj");
    assert!(status.success(), "jj command failed");
}

/// A git-only repository with the given remotes, the shape an agent's `/tmp`
/// clone has. Identity and `init.defaultBranch` are pinned as every lab git is.
pub fn git_repository(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create repository");
    git(root, "main", ["init", "--quiet"]);
    for (name, url) in remotes {
        git(root, "main", ["remote", "add", name, url]);
    }
}

/// `git init --bare` at `root`: the shape of a remote several machines share.
pub fn git_bare_repository(root: &Path) {
    let parent = root.parent().expect("repository path has a parent");
    std::fs::create_dir_all(parent).expect("create repository parent");
    git(
        parent,
        "main",
        [
            "init",
            "--quiet",
            "--bare",
            root.to_str().expect("utf-8 path"),
        ],
    );
}

/// A bare remote at `ledger.git` under a fresh directory, which also holds
/// whatever machines share it.
pub fn bare_remote() -> (TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("remote directory");
    let remote = root.path().join("ledger.git");
    git_bare_repository(&remote);
    (root, remote)
}

/// Push to `remote`, as `machine`'s ref, one commit carrying `contents` at
/// `path` under the ledger root: what a machine running another knives, or
/// a broken one, could send.
pub fn push_as_peer(remote: &Path, machine: &str, path: &str, contents: &str) {
    let scratch = tempfile::tempdir().expect("peer checkout");
    let root = scratch.path().join("peer");
    git_repository(&root, &[]);
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().expect("an entry's directory")).expect("mkdir");
    std::fs::write(&file, contents).expect("write the peer's entry");
    git_output(&root, ["add", "--all"]);
    git_output(&root, ["commit", "--quiet", "-m", "peer"]);
    git_output(
        &root,
        [
            "push",
            "--quiet",
            remote.to_str().expect("utf-8"),
            &format!("HEAD:refs/knives/{machine}"),
        ],
    );
}

/// Make a git directory at `git_dir` that joins the ledger as the dotfiles
/// installer does on a machine: a non-bare repository whose work tree is the
/// ledger root `work_tree`, whose `origin` is `url`, reached at `reached`,
/// committing as `machine`. Nothing is created at `work_tree`.
#[allow(
    clippy::too_many_arguments,
    reason = "the git directory, the work tree, the URL, where it is reached and the machine are independent inputs"
)]
pub fn join_ledger(git_dir: &Path, work_tree: &Path, url: &str, reached: &Path, machine: &str) {
    git_bare_repository(git_dir);
    let work_tree = work_tree.to_str().expect("utf-8 path");
    let instead_of = format!("url.{}.insteadOf", reached.display());
    for args in [
        ["config", "core.bare", "false"].as_slice(),
        &["config", "core.worktree", work_tree],
        &["remote", "add", "origin", url],
        &["config", &instead_of, url],
        &["config", "knives.machine", machine],
    ] {
        let status = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} in {}", git_dir.display());
    }
}

/// Whether `path` names a ledger entry file.
pub fn is_entry(path: &str) -> bool {
    Path::new(path).extension() == Some(std::ffi::OsStr::new("md"))
}

/// git's trimmed stdout in `directory`, with the lab's identity pinned as
/// every lab git is; the command must succeed.
pub fn git_output<const N: usize>(directory: &Path, args: [&str; N]) -> String {
    let output = git_command(directory, "main")
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 git output")
        .trim()
        .to_owned()
}

/// A colocated jj checkout with the given remotes: what the scan looks for.
pub fn jj_checkout(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create checkout");
    jj(root, ["git", "init", "--colocate"]);
    for (name, url) in remotes {
        jj(root, ["git", "remote", "add", name, url]);
    }
}

/// Stage and commit everything under `root`, `.jj` stores included: git refuses
/// only `.git` path components.
pub fn git_commit_all(root: &Path, message: &str) {
    git(root, "main", ["add", "-A"]);
    git(root, "main", ["commit", "--quiet", "-m", message]);
}

/// `git clone source destination`, quietly.
pub fn git_clone(source: &Path, destination: &Path) {
    let parent = destination
        .parent()
        .expect("clone destination has a parent");
    std::fs::create_dir_all(parent).expect("clone parent");
    git(
        parent,
        "main",
        [
            "clone",
            "--quiet",
            source.to_str().expect("utf-8"),
            destination.to_str().expect("utf-8"),
        ],
    );
}

/// `git remote set-url name url` in `root`.
pub fn git_set_remote_url(root: &Path, name: &str, url: &str) {
    git(root, "main", ["remote", "set-url", name, url]);
}

/// `jj workspace add --name name path` from `checkout`.
pub fn jj_workspace_add(checkout: &Path, name: &str, path: &Path) {
    jj(
        checkout,
        [
            "workspace",
            "add",
            "--name",
            name,
            path.to_str().expect("utf-8"),
        ],
    );
}

/// `command` with one rule handed to git as `GIT_CONFIG_COUNT` and its
/// numbered pair, as a session hands git configuration of its own.
pub fn with_git_rule<'c>(command: &'c mut Command, key: &str, value: &str) -> &'c mut Command {
    command
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", key)
        .env("GIT_CONFIG_VALUE_0", value)
}

/// The knives binary, run from `cwd` against the registry in `config_home` and
/// scanning `scan_home` for checkouts. Like everything a test starts, it sees
/// no harness session: the test process has none ([`isolation`]). Returned
/// unrun so a caller can add an environment variable (`KNIVES_OWNER`, say)
/// before `.output()`.
pub fn knives_command(cwd: &Path, config_home: &Path, scan_home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(args)
        .current_dir(cwd)
        .env("KNIVES_CONFIG_HOME", config_home)
        .env("HOME", scan_home);
    command
}

/// [`knives_command`] from the lab's work checkout, run to completion.
pub fn knives(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    knives_command(&lab.work, home.path(), lab.temp_path(), args)
        .output()
        .expect("run knives")
}

/// Whether the process `pid` has the file at `path` open, as Linux's `/proc`
/// shows it.
pub fn has_open(pid: u32, path: &Path) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|to| to == path))
    })
}

/// Wait until `child` waits for the lock whose file is `lock`, or has
/// exited; whether it waits. It waits while it holds the file open and
/// another holds the lock: a knives command keeps a lock's file open for
/// the whole wait, so a test sees the wait itself, with no clock. A knives
/// command gives up a lock it has waited a minute for, and exits; the poll's
/// own bound, four minutes, ends a child that hangs elsewhere as a failure,
/// never a verdict.
pub fn waits_for_a_lock(child: &mut std::process::Child, lock: &Path) -> bool {
    for _ in 0..12_000 {
        if has_open(child.id(), lock) && !lock_is_free(lock) {
            return true;
        }
        if child.try_wait().expect("poll the command").is_some() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    panic!("the command neither waited for a lock nor exited in four minutes");
}

/// Whether nobody holds the lock whose file is `path`: nobody ever made it,
/// or a try of it from here takes it, and lets it go at once. A test whose
/// path is wrong reads it as free, so each test that asks this has a control
/// that holds the lock at that path.
pub fn lock_is_free(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open the lock file")
        .try_lock()
        .is_ok()
}

/// A program of the name of `real`, first on a `PATH` of its own, that holds
/// each call whose arguments, spelled ` $* `, match a shell pattern, in a
/// process whose `LAB_HOLD_GATE` names a gate file: it writes the gate's
/// name with `.waiting` after it, and waits (up to four minutes, past the
/// minute a waiter gives a lock) for the gate to exist, then runs `real`. A
/// process without `LAB_HOLD_GATE` runs `real` at once: a network call that
/// stalls, for one command.
pub struct Holding {
    root: TempDir,
    path: std::ffi::OsString,
}

impl Holding {
    /// Hold each call of `real` whose ` $* ` matches `pattern`.
    pub fn new(real: &Path, pattern: &str) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("holding directory");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).expect("holding bin directory");
        let name = real.file_name().expect("a program has a file name");
        let script = bin.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 case \" $* \" in\n\
                 {pattern})\n\
                 \x20 if [ -n \"$LAB_HOLD_GATE\" ]; then\n\
                 \x20   : > \"$LAB_HOLD_GATE.waiting\"\n\
                 \x20   i=0\n\
                 \x20   while [ ! -e \"$LAB_HOLD_GATE\" ] && [ $i -lt 4800 ]; do sleep 0.05; i=$((i+1)); done\n\
                 \x20 fi;;\n\
                 esac\n\
                 exec '{}' \"$@\"\n",
                real.display()
            ),
        )
        .expect("write the holding program");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make the holding program executable");
        let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH"),
        )))
        .expect("join PATH");
        Self { root, path }
    }

    /// Hold each `git` call whose ` $* ` matches `pattern`.
    pub fn git(pattern: &str) -> Self {
        Self::new(&on_path("git"), pattern)
    }

    /// The `PATH` with this program first.
    pub fn path(&self) -> &std::ffi::OsStr {
        &self.path
    }

    fn gate(&self) -> PathBuf {
        self.root.path().join("gate")
    }

    fn waiting(&self) -> PathBuf {
        self.root.path().join("gate.waiting")
    }

    /// Spawn `command` with this program first on its `PATH` and its calls
    /// held: the child, once one call waits at the gate, or the child's
    /// output where it exited first.
    pub fn hold(&self, command: &mut Command) -> Result<std::process::Child, std::process::Output> {
        let mut child = command
            .env("PATH", &self.path)
            .env("LAB_HOLD_GATE", self.gate())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the held command");
        for _ in 0..12_000 {
            if self.waiting().exists() {
                return Ok(child);
            }
            if child.try_wait().expect("poll the held command").is_some() {
                return Err(child.wait_with_output().expect("collect the held command"));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        panic!("the held command neither reached its gate nor exited in four minutes");
    }

    /// Open the gate, and collect `child`.
    pub fn release(&self, child: std::process::Child) -> Output {
        std::fs::write(self.gate(), "").expect("open the gate");
        child.wait_with_output().expect("collect the held command")
    }
}

/// Where `program` is on this process's `PATH`.
pub fn on_path(program: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{program} on PATH"))
}

/// A `gh`, in a directory of its own, that answers what a release plan asks
/// the forge of a registered consumer, `acme/consumer`: its default branch's
/// head, and no pin file there ([`register_consumer`]).
pub struct ConsumerForge {
    root: TempDir,
}

impl ConsumerForge {
    pub fn answering() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("forge directory");
        let gh = root.path().join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *\" graphql \"*)\n\
             \x20 echo '{\"data\":{\"repository\":{\"defaultBranchRef\":{\"name\":\"main\",\
             \"target\":{\"oid\":\"1111111111111111111111111111111111111111\"}}}}}';;\n\
             *\" repos/acme/consumer/contents/\"*)\n\
             \x20 echo 'gh: Not Found (HTTP 404)' >&2\n\
             \x20 exit 1;;\n\
             *)\n\
             \x20 echo \"unexpected gh $*\" >&2\n\
             \x20 exit 1;;\n\
             esac\n",
        )
        .expect("write the forge's gh");
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
            .expect("make the forge's gh executable");
        Self { root }
    }

    /// The forge's `gh`.
    pub fn gh(&self) -> PathBuf {
        self.root.path().join("gh")
    }

    /// A cache directory of the forge's own, for `XDG_CACHE_HOME`.
    pub fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }
}

/// Register `acme/consumer` as a consumer of the lab fork in `home`'s
/// registry, whose one table is the fork's ([`ConsumerForge`]).
pub fn register_consumer(home: &Path) {
    let registry = home.join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read the registry");
    text.push_str("consumers = [\"acme/consumer\"]\n");
    std::fs::write(&registry, text).expect("write the registry");
}

/// What Claude Code's `SessionStart` hook prints for session `session_id`
/// started in `cwd`, against the registry in `home`.
///
/// Not built on [`knives_command`], which also sets the working directory and
/// removes `KNIVES_OWNER`, a variable the hook reads.
pub fn session_start(home: &Path, cwd: &Path, session_id: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "claude-code"])
        .env("KNIVES_CONFIG_HOME", home)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn hook");
    let event = serde_json::json!({
        "session_id": session_id,
        "hook_event_name": "SessionStart",
        "source": "startup",
        "cwd": cwd,
    });
    std::io::Write::write_all(
        &mut child.stdin.take().expect("hook stdin"),
        event.to_string().as_bytes(),
    )
    .expect("write hook input");
    child.wait_with_output().expect("wait for hook")
}

/// One note an older knives filed in `home`'s ledger directory `fork`,
/// written at `ts`; the path of its file. The entry is built from its
/// serialized form, so this compiles against every knives whatever fields
/// a later one adds.
pub fn note(home: &Path, fork: &str, ts: &str, text: &str) -> PathBuf {
    let entry: knives::ledger::Entry = serde_json::from_value(serde_json::json!({
        "ts": ts,
        "owner": "ses_older",
        "kind": knives::ledger::Kind::Note,
        "text": text,
    }))
    .expect("build a note");
    knives::ledger::Ledger::at(home.join("ledger").join(fork))
        .append(&entry)
        .expect("append a note")
}

/// State `kind` about `branch` on `repo`'s ledger in the config home `home`,
/// through the same scribe `track`, `depends` and `finish --superseded-by`
/// write with. A store opened afterwards at `home/state.json` answers for it.
#[allow(
    clippy::too_many_arguments,
    reason = "a fixture: where the ledger is, whose it is, and the one statement it writes are independent"
)]
pub fn state_on_ledger(
    home: &Path,
    repo: &str,
    branch: &str,
    kind: knives::statement::StatementKind,
    value: Option<&str>,
) {
    knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(home.join("ledger").join(repo)),
        knives::ids::UpstreamName::new(repo),
        home.to_owned(),
        "a-test".to_owned(),
    )
    .record(&knives::ledger::Draft {
        statement: Some(knives::ledger::Stated {
            statement: knives::statement::Statement {
                kind,
                value: value.map(str::to_owned),
            },
            replaces: None,
        }),
        ..knives::ledger::Draft::event(Some(branch), "stated".to_owned(), None)
    })
    .expect("state on the ledger");
}

fn jj_output<const N: usize>(directory: &Path, args: [&str; N]) -> String {
    let output = Command::new("jj")
        .args(args)
        .current_dir(directory)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 jj output")
        .trim()
        .to_owned()
}

impl Lab {
    /// Run a jj command in the first clone, for tests that need to set up a
    /// specific working-copy state.
    pub(crate) fn jj_work<const N: usize>(&self, args: [&str; N]) {
        jj(&self.work, args);
    }

    /// [`Self::jj_work`], returning trimmed stdout.
    pub(crate) fn jj_work_output<const N: usize>(&self, args: [&str; N]) -> String {
        jj_output(&self.work, args)
    }

    /// Run a jj command in an explicitly selected workspace.
    pub(crate) fn jj_at<const N: usize>(&self, directory: &Path, args: [&str; N]) {
        jj(directory, args);
    }

    pub(crate) fn fetch_work(&self) {
        jj(&self.work, ["git", "fetch", "--all-remotes"]);
    }
}

/// Repeated status runs carry their measured forge duration in the headline.
/// Compare the reported facts while retaining the distinct `forge` state token.
pub fn without_forge_elapsed(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.contains("  forge ") {
                let (headline, _) = line
                    .rsplit_once(" (")
                    .expect("a status headline ends with elapsed milliseconds");
                format!("{headline} (elapsed)")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The newest operation's description.
pub fn newest_operation_description(repo: &Path) -> String {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "--limit",
            "1",
            "-T",
            "description",
        ])
        .current_dir(repo)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read newest operation");
    assert!(output.status.success(), "read newest operation");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Add a commit to `branch`, leave its bookmark on the new tip, and step off it.
pub fn extend_branch(lab: &Lab, branch: &str, file: &str, content: &str) {
    lab.jj_work(["new", branch, "-m", "follow-up"]);
    std::fs::write(lab.work.join(file), content).expect("extend a branch");
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

pub fn file_at_revision(lab: &Lab, revision: &str, file: &str) -> String {
    let output = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            revision,
            &format!("root:{file}"),
        ])
        .output()
        .expect("show revision file");
    assert!(
        output.status.success(),
        "file show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 file content")
}

/// [`release_test_home`], with `release/2026-08-04` already cut from whatever
/// branches `lab` carries: the starting point of every release-edit test.
pub fn home_after_first_cut(lab: &Lab) -> (tempfile::TempDir, PathBuf) {
    let (home, consumer) = release_test_home(lab);
    let cut = knives_release(lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    (home, consumer)
}

/// Both streams of `output`, for an assertion's message.
pub fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The standard output of `output` alone.
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// [`jj`] for an argument list of any length. The command must succeed.
pub fn jj_in(directory: &Path, args: &[&str]) {
    let status = Command::new("jj")
        .args(args)
        .current_dir(directory)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("run jj");
    assert!(status.success(), "jj {args:?} failed");
}

/// A lab whose trunk, upstream and on the fork, holds `files`: the base every
/// member forks from.
pub fn trunk_with(files: &[(&str, &str)]) -> Lab {
    let lab = Lab::new();
    for (file, content) in files {
        lab.upstream_trunk_file(file, content);
    }
    lab.mirror_upstream_trunk_to_origin();
    lab
}

/// A release verb run in the work checkout that must exit 0; its stdout.
pub fn release_ok(lab: &Lab, home: &TempDir, args: &[&str]) -> String {
    let output = knives_release(lab, home, args);
    assert_eq!(output.status.code(), Some(0), "{args:?}: {}", text(&output));
    stdout(&output)
}

/// A release verb run in the work checkout that must go through, though the
/// plan around it may exit with findings of its own; its stdout.
pub fn release_ran(lab: &Lab, home: &TempDir, args: &[&str]) -> String {
    release_ran_in(lab, home, &lab.work, args)
}

/// [`release_ran`] from `cwd`, a workspace of the work checkout, say.
pub fn release_ran_in(lab: &Lab, home: &TempDir, cwd: &Path, args: &[&str]) -> String {
    let output = release_command(lab, home, ReleaseOutput::Text, args)
        .current_dir(cwd)
        .output()
        .expect("run knives release");
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{args:?}: {}",
        text(&output)
    );
    stdout(&output)
}

/// Whether `commit` is still a visible commit of the work checkout.
pub fn is_visible(lab: &Lab, commit: &knives::ids::CommitId) -> bool {
    knives::jj::commits_matching(&lab.work, "all()")
        .expect("list visible commits")
        .contains(commit)
}

/// The forge repository the lab fork's upstream is reached by where two
/// machines share a ledger, so the ledger keeps the fork under its
/// `<owner>/<name>`: a filesystem upstream names no repository that another
/// machine could share a ledger for.
pub const UPSTREAM: &str = "https://forge.invalid/acme/demo";
/// The repository each machine's registry entry says its ledger belongs to.
pub const LEDGER: &str = "acme/ledger";
/// [`LEDGER`] as each ledger root's `origin` spells it; git reaches it at a
/// bare remote through `insteadOf`.
pub const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

/// A config home whose registry calls the lab fork `demo`, with [`UPSTREAM`]
/// as its upstream and its ledger shared with [`LEDGER`], and the lab's work
/// checkout pointing there too.
pub fn shared_home(lab: &Lab) -> TempDir {
    let (home, _consumer) = release_test_home(lab);
    lab.upstream_at_forge_url(UPSTREAM);
    let registry = home.path().join("repos.toml");
    let registered = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{}ledger = \"{LEDGER}\"\n",
            registered.replace(&lab.upstream.display().to_string(), UPSTREAM)
        ),
    )
    .expect("write the registry");
    home
}

/// Make `home`'s ledger root a repository whose `origin` is [`LEDGER_URL`],
/// reached at `remote`, committing as `machine`.
pub fn share(home: &Path, machine: &str, remote: &Path) {
    let root = home.join("ledger");
    git_repository(&root, &[("origin", LEDGER_URL)]);
    let reached = format!("url.{}.insteadOf", remote.display());
    git_output(&root, ["config", reached.as_str(), LEDGER_URL]);
    git_output(&root, ["config", "knives.machine", machine]);
}

/// A second machine's config home: the same registry, nothing else yet.
pub fn second_machine(first: &Path) -> TempDir {
    let home = tempfile::tempdir().expect("second config home");
    std::fs::copy(first.join("repos.toml"), home.path().join("repos.toml"))
        .expect("copy the registry");
    home
}

/// A config home whose registry releases the lab fork `demo` on the fixed
/// branch `branch`, with an empty local consumer.
pub fn fixed_home(lab: &Lab, branch: &str) -> TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             release_branch = \"{branch}\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let consumer = home.path().join("consumer");
    std::fs::create_dir_all(&consumer).expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.display().to_string(),
    )
    .expect("write local consumer fixture path");
    home
}

/// The fork recipe's fast-forward, as `tests/release_unrecorded.rs` makes
/// it: the release `name` rebased onto its parents and the publish remote's
/// commit of it, its tree put back.
pub fn fast_forward(lab: &Lab, name: &str) {
    let before = commit_at(lab, name);
    let destination = format!("parents({name}) | {name}@origin");
    lab.jj_work(["rebase", "-r", name, "-d", destination.as_str()]);
    lab.jj_work(["restore", "--from", before.as_str(), "--into", name]);
}

/// Upstream squash-merges `branch`'s pull request `number`. Returns the
/// squash commit.
pub fn squash_land(lab: &Lab, branch: &str, number: u64) -> knives::ids::CommitId {
    lab.publish_pull(branch, number);
    lab.squash_merge_pull(number, None);
    commit_at(lab, "main@upstream")
}

/// The files `revision` leaves conflicted in the work checkout.
pub fn conflicts_of(lab: &Lab, revision: &str) -> Vec<String> {
    knives::jj::conflicted_files(&lab.work, revision).expect("list the conflicted files")
}

/// Edit `release` by hand, as a person does: `jj edit`, the files written,
/// the working copy moved off it.
pub fn edit_by_hand(lab: &Lab, release: &str, files: &[(&str, &str)]) {
    lab.jj_work(["edit", release]);
    for (file, content) in files {
        std::fs::write(lab.work.join(file), content).expect("write by hand");
    }
    lab.jj_work(["new", "main@origin"]);
}

/// `knives release adopt <release> --why <why>`, which must record the
/// release as its newest write.
pub fn adopt(lab: &Lab, home: &TempDir, release: &str, why: &str) {
    let adopted = release_ran(lab, home, &["adopt", release, "--why", why]);
    assert!(
        adopted.contains(&format!("as the newest write of {release}")),
        "{adopted}"
    );
}

/// The release of INFRA-4895's drop-after-adopt probe ([`included_release`]).
const PROBE_RELEASE: &str = "release/2026-08-04";
/// The probe's member that sets lines 5 and 6 of `f`, as `d809` did.
const PROBE_D: &str = "feat/d";
/// The probe's member, made after the first cut, that sets line 5 of `f` as
/// [`PROBE_D`] does.
const PROBE_A: &str = "feat/a";

/// The probe's `f`: thirty lines `let lN = N`, with each line of `changed`
/// set to its value.
pub fn f_with(changed: &[(usize, &str)]) -> String {
    use std::fmt::Write as _;
    let mut f = String::new();
    for line in 1..=30 {
        match changed.iter().find(|(at, _)| *at == line) {
            Some((_, value)) => writeln!(f, "let l{line} = {value}"),
            None => writeln!(f, "let l{line} = {line}"),
        }
        .expect("write to a String");
    }
    f
}

/// The probe's D's `f`: A's change to line 5, and its own line 6 next to it.
pub fn d_f() -> String {
    f_with(&[(5, "compare_fix ()"), (6, "d809_only ()")])
}

/// The probe's A's `f`: line 5 only.
pub fn a_f() -> String {
    f_with(&[(5, "compare_fix ()")])
}

/// The probe's shape up to its include: a trunk holding `f` ([`f_with`]);
/// `feat/m` adds `m.txt`; [`PROBE_D`] writes `d` as its `f`; a first cut
/// [`PROBE_RELEASE`] of the two; [`PROBE_A`], made after the cut, writes
/// [`a_f`]; `include feat/a`, which must exit 0. Returns the lab, its home
/// and the include's stdout.
pub fn included_release(d: &str) -> (Lab, TempDir, String) {
    let lab = trunk_with(&[("f", &f_with(&[]))]);
    lab.branch("feat/m", "m.txt", "m\n");
    lab.branch(PROBE_D, "f", d);
    let (home, _consumer) = release_test_home(&lab);
    let cut = release_ran(&lab, &home, &["cut", PROBE_RELEASE]);
    assert_eq!(release_parents(&lab, PROBE_RELEASE).len(), 2, "{cut}");
    lab.branch(PROBE_A, "f", &a_f());
    let include = release_ok(&lab, &home, &["include", PROBE_A]);
    (lab, home, include)
}

/// How `f` is resolved by hand after the probe's `include feat/a`
/// conflicts there ([`adopted_release`]).
#[derive(Clone, Copy)]
pub enum Resolution {
    /// `f` as [`PROBE_D`] has it, which holds A's line too.
    DVersion,
    /// A text that keeps neither side.
    Neither,
}

/// [`included_release`] with [`d_f`]: the include conflicts in `f`, which is
/// resolved by hand as `resolution` says and adopted. Returns the lab and
/// its home.
pub fn adopted_release(resolution: Resolution) -> (Lab, TempDir) {
    let (lab, home, include) = included_release(&d_f());
    assert_eq!(conflicts_of(&lab, PROBE_RELEASE), ["f"], "{include}");
    match resolution {
        Resolution::DVersion => {
            lab.jj_work(["restore", "--from", PROBE_D, "--into", PROBE_RELEASE, "f"]);
        }
        Resolution::Neither => edit_by_hand(&lab, PROBE_RELEASE, &[("f", "merged\n")]),
    }
    adopt(&lab, &home, PROBE_RELEASE, "resolve f");
    assert!(
        conflicts_of(&lab, PROBE_RELEASE).is_empty(),
        "the adopt left a conflict"
    );
    (lab, home)
}
