#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]
#![allow(
    clippy::unused_self,
    unreachable_pub,
    reason = "a test fixture: method form keeps call sites readable, and this module is included by path"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

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
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
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

    pub(crate) fn reset_consumer_to_origin(&self, consumer: &Path) {
        let origin_trunk = format!("origin/{}", self.trunk);
        git(
            consumer,
            &self.trunk,
            ["reset", "--hard", origin_trunk.as_str()],
        );
    }

    pub(crate) fn rename_consumer_remote(&self, consumer: &Path, from: &str, to: &str) {
        git(consumer, &self.trunk, ["remote", "rename", from, to]);
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

    pub(crate) fn rewrite_in_both_clones(&self, branch: &str) {
        self.push_branch(branch);
        jj(&self.second, ["git", "fetch", "--remote", "origin"]);
        let remote_branch = format!("{branch}@origin");
        jj(&self.second, ["bookmark", "track", &remote_branch]);
        jj(&self.work, ["edit", "--ignore-immutable", branch]);
        std::fs::write(self.work.join("feature.txt"), "work rewrite\n")
            .expect("rewrite first clone");
        jj(&self.work, ["bookmark", "set", branch, "-r", "@"]);
        jj(&self.work, ["new"]);
        jj(&self.second, ["edit", "--ignore-immutable", branch]);
        std::fs::write(self.second.join("feature.txt"), "second rewrite\n")
            .expect("rewrite second clone");
        jj(&self.second, ["bookmark", "set", branch, "-r", "@"]);
        jj(&self.second, ["new"]);
        jj(
            &self.second,
            ["git", "push", "--remote", "origin", "--bookmark", branch],
        );
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
        }
    }
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

fn jj<const N: usize>(directory: &Path, args: [&str; N]) {
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

    pub(crate) fn fetch_work(&self) {
        jj(&self.work, ["git", "fetch", "--all-remotes"]);
    }
}
