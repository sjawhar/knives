//! A fork checkout whose remotes name three owners — its upstream's, its fork's and
//! its release's, as a real fork's do — names the forge repository on every call
//! `audit` and `status` make, so a gh that refuses a call naming none (a wrapper
//! that holds one owner's token, and cannot tell from such remotes whose to send)
//! never stands between them and the pull requests.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;

use forge_shim::{install_repository_requiring_gh, path_with_gh_shim, pull_record_with_fields};
use lab::Lab;

const UPSTREAM: &str = "https://github.com/upstream-owner/demo.git";
const ORIGIN: &str = "https://github.com/fork-owner/demo.git";
const RELEASE: &str = "https://github.com/release-owner/demo.git";

/// A lab fork with one pushed branch whose `upstream`, `origin` and `release`
/// remotes name three owners' repositories (git reads each from the lab's own
/// bare repositories), a registry naming the same three, and the gate in front of
/// the snapshot shim answering one open pull request of the branch.
struct ThreeOwnerFork {
    lab: Lab,
    home: tempfile::TempDir,
    shim: tempfile::TempDir,
}

impl ThreeOwnerFork {
    fn new() -> Self {
        let lab = Lab::new();
        lab.branch("feat/alpha", "alpha.txt", "alpha\n");
        lab.push_branch("feat/alpha");
        lab.upstream_at_forge_url(UPSTREAM);
        lab.origin_at_forge_url(ORIGIN);
        let rewrite = format!("url.{}.insteadOf", lab.temp_origin().display());
        lab::git_output(&lab.work, ["config", "--add", &rewrite, RELEASE]);
        lab::git_output(&lab.work, ["remote", "add", "release", RELEASE]);
        lab::jj(&lab.work, ["git", "fetch", "--remote", "release"]);

        let home = tempfile::tempdir().expect("create config home");
        std::fs::write(
            home.path().join("repos.toml"),
            format!(
                "[repos.demo]\nupstream = \"{UPSTREAM}\"\norigin = \"{ORIGIN}\"\n\
                 release = \"{RELEASE}\"\n"
            ),
        )
        .expect("write registry");

        let pulls = format!(
            "[{}]",
            pull_record_with_fields(
                7,
                "OPEN",
                "feat/alpha",
                r#","headRepositoryOwner":{"login":"fork-owner"},"additions":1,"deletions":0,"changedFiles":1,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","headRef":{"name":"feat/alpha"}"#,
            )
        );
        let shim = tempfile::tempdir().expect("create forge shim directory");
        install_repository_requiring_gh(shim.path(), &pulls, &shim.path().join("calls.log"));
        Self { lab, home, shim }
    }

    /// `knives <args>` from the fork's checkout, with the gate first on PATH.
    fn knives(&self, args: &[&str]) -> std::process::Output {
        lab::knives_command(&self.lab.work, self.home.path(), self.lab.temp_path(), args)
            .env("XDG_CACHE_HOME", self.shim.path().join("cache"))
            .env("PATH", path_with_gh_shim(self.shim.path()))
            .output()
            .expect("run knives behind the gate")
    }

    /// Every gh call the gate saw, one line of arguments each, refusals marked.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.shim.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn exit(output: &std::process::Output) -> Option<i32> {
    output.status.code()
}

fn answered() -> [Option<i32>; 2] {
    [knives::cli::Exit::Ok, knives::cli::Exit::Findings].map(|exit| Some(i32::from(exit.code())))
}

/// The calls the gate saw: none refused, the identity asked of the upstream's
/// repository, and the one cold discovery's lists (the base window, and one per
/// owner of origin and release) of the repository the forge answered for it.
fn assert_every_call_named_its_repository(calls: &[String]) {
    assert!(
        calls.iter().all(|call| !call.starts_with("REFUSED ")),
        "a gh call named no repository: {calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("repo view upstream-owner/demo ")),
        "the identity was not asked of the upstream's repository: {calls:#?}"
    );
    let lists: Vec<&String> = calls
        .iter()
        .filter(|call| call.starts_with("pr list "))
        .collect();
    assert_eq!(lists.len(), 3, "one cold discovery's lists: {calls:#?}");
    assert!(
        lists
            .iter()
            .all(|list| list.starts_with("pr list -R fake-owner/fake-repo ")),
        "a pull-request list did not name the forge's repository: {lists:#?}"
    );
    for author in ["author:fork-owner", "author:release-owner"] {
        assert!(
            lists.iter().any(|list| list.contains(author)),
            "no {author} list: {lists:#?}"
        );
    }
}

#[test]
fn audit_in_a_fork_whose_remotes_name_three_owners_names_the_repository_on_every_gh_call() {
    // Given: a fork checkout with three remotes of three owners, behind a gh that
    // refuses any call naming no repository.
    let fork = ThreeOwnerFork::new();

    // When: the audit runs against the forge.
    let output = fork.knives(&["--json", "audit"]);

    // Then: every call named its repository, so the audit answered the open pull
    // request's head on its own merits, with no problem from the forge.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let calls = fork.calls();
    assert_every_call_named_its_repository(&calls);
    assert!(
        answered().contains(&exit(&output)),
        "exit {:?}\nstdout: {stdout}\nstderr: {stderr}\ncalls: {calls:#?}",
        exit(&output)
    );
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("audit prints JSON");
    assert_eq!(report.get("problems"), None, "stdout: {stdout}");
    let row = report["branches"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .unwrap_or_else(|| panic!("no feat/alpha row: {stdout}"));
    assert_eq!(row["pull"]["number"], 7, "stdout: {stdout}");
}

#[test]
fn status_in_a_fork_whose_remotes_name_three_owners_names_the_repository_on_every_gh_call() {
    // Given: a fork checkout with three remotes of three owners, behind a gh that
    // refuses any call naming no repository.
    let fork = ThreeOwnerFork::new();

    // When: status runs against the forge, cold and then warm from its cache.
    let cold = fork.knives(&["--text", "status", "--no-landed"]);
    let warm = fork.knives(&["--text", "status", "--no-landed"]);

    // Then: every call of both runs named its repository, and both read the pull
    // request's state from the forge.
    let calls = fork.calls();
    assert_every_call_named_its_repository(&calls);
    assert!(
        calls.iter().any(|call| call.starts_with("api graphql ")
            && call.contains("-f owner=fake-owner")
            && call.contains("orderBy: {field: UPDATED_AT")),
        "the warm run did not sweep the forge's repository: {calls:#?}"
    );
    for output in [&cold, &warm] {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            answered().contains(&exit(output)),
            "exit {:?}\nstdout: {stdout}\nstderr: {stderr}\ncalls: {calls:#?}",
            exit(output)
        );
        assert!(stdout.contains("forge consulted"), "stdout: {stdout}");
        assert!(
            !stdout.contains("pull request state unavailable"),
            "stdout: {stdout}"
        );
        assert!(stdout.contains("#7"), "stdout: {stdout}");
    }
}
