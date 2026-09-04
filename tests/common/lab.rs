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
use std::process::Command;

use tempfile::TempDir;

/// Point every jj in this test process at a config home under cargo's target
/// directory, before `main` runs.
///
/// jj resolves a repository's config to `$XDG_CONFIG_HOME/jj/repos/<config-id>/`
/// and creates that directory on first contact; left unset, every lab repository
/// leaves one in the developer's real `~/.config`. Per-spawn `.env()` is not
/// enough: tests also call knives in-process, and the jj it spawns inherits the
/// test process's own environment. One variable for the whole process keeps the
/// lab's jj, the knives binary, and in-process knives agreeing on where a
/// repository's config (identity, `immutable_heads()`) lives.
#[ctor::ctor(unsafe)]
fn isolate_jj_config_home() {
    let xdg = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xdg");
    // SAFETY: a constructor runs before `main`, hence before libtest spawns its
    // first thread; nothing can read the environment concurrently.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg) }
}

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
        let temp = TempDir::new().expect("create lab directory");
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
            path: self.work.clone(),
            upstream: self.upstream.display().to_string(),
            // The work directory deliberately stands in for origin, per lab convention.
            origin: self.work.display().to_string(),
            base: None,
            release: None,
            release_branch: Some(name.to_owned()),
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        }
    }
}
/// A registry entry for the lab's work checkout, which stands in for origin.
pub fn lab_entry(lab: &Lab) -> knives::config::RepoEntry {
    knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
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
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n",
            lab.work.display(),
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
        .env("JJ_CONFIG", "/dev/null")
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
        .env("KNIVES_CONFIG_HOME", home.path());
    command
}

/// `knives start <branch> --repo demo --why test` from the work checkout, for
/// callers that add an environment variable before running it.
pub fn start_command(lab: &Lab, home: &tempfile::TempDir, branch: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(["--text", "start", branch, "--repo", "demo", "--why", "test"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path());
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
    let status = Command::new("git")
        .args(args)
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
        .env("GIT_CONFIG_VALUE_0", trunk)
        .status()
        .expect("run git");
    assert!(status.success(), "git command failed");
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
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("run jj");
    assert!(status.success(), "jj command failed");
}

fn jj_output<const N: usize>(directory: &Path, args: [&str; N]) -> String {
    let output = Command::new("jj")
        .args(args)
        .current_dir(directory)
        .env("JJ_CONFIG", "/dev/null")
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
        .env("JJ_CONFIG", "/dev/null")
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
        .env("JJ_CONFIG", "/dev/null")
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
