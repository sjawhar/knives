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
    _temp: TempDir,
    pub(crate) upstream: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) second: PathBuf,
    pub(crate) maintainer: PathBuf,
}

impl Lab {
    pub(crate) fn new() -> Self {
        let temp = TempDir::new().expect("create lab directory");
        let upstream = temp.path().join("upstream.git");
        let origin = temp.path().join("origin.git");
        let seed = temp.path().join("seed");
        let maintainer = temp.path().join("maintainer");
        let work = temp.path().join("work");
        let second = temp.path().join("second");

        git(
            temp.path(),
            ["init", "--bare", upstream.to_str().expect("utf-8 path")],
        );
        git(
            temp.path(),
            ["init", "--bare", origin.to_str().expect("utf-8 path")],
        );
        git(
            temp.path(),
            [
                "clone",
                upstream.to_str().expect("utf-8 path"),
                seed.to_str().expect("utf-8 path"),
            ],
        );
        std::fs::write(seed.join("README.md"), "seed\n").expect("write seed");
        git(&seed, ["add", "README.md"]);
        git(&seed, ["commit", "-m", "seed"]);
        git(&seed, ["branch", "-M", "main"]);
        git(&seed, ["push", "origin", "main"]);
        git(
            &seed,
            [
                "remote",
                "add",
                "fork",
                origin.to_str().expect("utf-8 path"),
            ],
        );
        git(&seed, ["push", "fork", "main"]);
        git(
            temp.path(),
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

        Self {
            _temp: temp,
            upstream,
            work,
            second,
            maintainer,
        }
    }

    pub(crate) fn branch(&self, name: &str, file: &str, content: &str) {
        jj(&self.work, ["new", "-r", "main@origin", "-m", name]);
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
        git(&self.work, ["push", "upstream", &destination]);
    }

    pub(crate) fn squash_merge_pull(&self, number: u64, changed_content: Option<&str>) {
        let pull = format!("refs/pull/{number}/head:refs/remotes/origin/pull/{number}/head");
        git(&self.maintainer, ["fetch", "origin", &pull]);
        git(&self.maintainer, ["checkout", "main"]);
        let pull = format!("origin/pull/{number}/head");
        git(&self.maintainer, ["merge", "--squash", &pull]);
        if let Some(content) = changed_content {
            std::fs::write(self.maintainer.join("feature.txt"), content)
                .expect("alter squashed content");
            git(&self.maintainer, ["add", "feature.txt"]);
        }
        git(&self.maintainer, ["commit", "-m", "squash merge"]);
        git(&self.maintainer, ["push", "origin", "main"]);
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    pub(crate) fn rebase_and_force_push(&self, branch: &str) {
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
        jj(&self.work, ["rebase", "-b", branch, "-o", "main@upstream"]);
        jj(
            &self.work,
            ["git", "push", "--remote", "origin", "--bookmark", branch],
        );
    }

    pub(crate) fn advance_upstream(&self, content: &str) {
        std::fs::write(self.maintainer.join("UPSTREAM.md"), content)
            .expect("write upstream advance");
        git(&self.maintainer, ["add", "UPSTREAM.md"]);
        git(&self.maintainer, ["commit", "-m", "advance upstream"]);
        git(&self.maintainer, ["push", "origin", "main"]);
        jj(&self.work, ["git", "fetch", "--remote", "upstream"]);
    }

    pub(crate) fn octopus(&self, name: &str, first: &str, second: &str) {
        jj(
            &self.work,
            [
                "new",
                "-r",
                "main@origin",
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
}

fn git<const N: usize>(directory: &Path, args: [&str; N]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_AUTHOR_NAME", "Knives Lab")
        .env("GIT_AUTHOR_EMAIL", "knives-lab@example.test")
        .env("GIT_COMMITTER_NAME", "Knives Lab")
        .env("GIT_COMMITTER_EMAIL", "knives-lab@example.test")
        // Pin the initial branch name instead of inheriting `init.defaultBranch` from
        // whoever is running the tests. Without this the lab's branch is `main` on a
        // machine that configures it and `master` on one that does not, so every push of
        // `main` fails with "src refspec main does not match any" — green locally, red in
        // CI, which is exactly the class of difference a test harness must not have.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "init.defaultBranch")
        .env("GIT_CONFIG_VALUE_0", "main")
        .status()
        .expect("run git");
    assert!(status.success(), "git command failed");
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
