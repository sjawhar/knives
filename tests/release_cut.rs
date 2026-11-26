//! `knives release cut`: a flat merge of every carried branch, and nothing else.
//!
//! The plan says when the release lags the trunk and which base is superseded.
//! The cut is one operation that carries its provenance, refuses a merge that
//! did not get the parents it asked for, audits members from their fork point,
//! returns findings for an inconclusive audit or a dropped test count, and
//! discards a failed candidate without trace. The fixed scheme cuts in place
//! and keeps its published position; a dated cut refuses a sideways move. A
//! cut that lacks a member only the newest recorded write holds is refused,
//! and the refusal says to move the release to that write.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::config::RepoEntry;
use knives::ids::ReleaseScheme;
use knives::jj::Repo;
use lab::{
    Lab, ReleaseOutput, commit_at, knives, knives_release, newest_operation_description,
    operation_ids, release_command, release_parents, release_test_home, release_test_home_pinned,
};

#[test]
fn a_first_cut_audits_members_from_their_fork_point_not_the_trunk_tip() {
    // Given: three branches forked from the seed, one of them squash-merged
    // upstream with maintainer edits that rewrite alpha's file. The first cut
    // has no previous composition, so its audit base is chosen from scratch.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the first cut merges every branch.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it succeeds, flat, with exactly the three branches as parents.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "first cut failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "parents: {parents:?}");
    assert!(
        !parents.contains(&commit_at(&lab, "main@upstream")),
        "the trunk must not be a parent"
    );
}

#[test]
fn release_plan_exits_with_findings_when_the_current_release_lags_the_upstream_trunk() {
    // Given: a clean dated release that was cut before upstream advanced.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan reports its warnings.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: scripts receive the findings exit code for the actionable trunk warning.
    assert!(
        text.contains("does not contain the upstream trunk"),
        "trunk lag not rendered: {text}"
    );
    assert_eq!(output.status.code(), Some(1), "stdout: {text}");
}

/// The files of a user jj config that signs the user's own commits with a
/// fresh ssh key, the way a signing policy is usually stated.
struct SigningConfig {
    /// The user config file, for `JJ_CONFIG`: the policy at the top level.
    config: std::path::PathBuf,
    /// One line per `ssh-keygen -Y sign` run: the signing program is a wrapper
    /// that records each signature before it runs the real `ssh-keygen`.
    signatures: std::path::PathBuf,
    directory: std::path::PathBuf,
    public_key: std::path::PathBuf,
    program: std::path::PathBuf,
    allowed: std::path::PathBuf,
}

impl SigningConfig {
    fn new(directory: &std::path::Path) -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        let key = directory.join("signing-key");
        let generated = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "knives-test", "-f"])
            .arg(&key)
            .status()
            .expect("run ssh-keygen");
        assert!(generated.success(), "ssh-keygen failed");
        let public_key = key.with_extension("pub");
        let public = std::fs::read_to_string(&public_key).expect("read public key");
        let allowed = directory.join("allowed-signers");
        std::fs::write(&allowed, format!("knives-lab@example.test {public}"))
            .expect("write allowed signers");
        let signatures = directory.join("signatures.log");
        let program = directory.join("signing-program");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 case \" $* \" in *\" -Y sign \"*) echo sign >> '{}' ;; esac\n\
                 exec ssh-keygen \"$@\"\n",
                signatures.display()
            ),
        )
        .expect("write signing program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make signing program executable");
        let mut signing = Self {
            config: directory.join("signing.toml"),
            signatures,
            directory: directory.to_owned(),
            public_key,
            program,
            allowed,
        };
        signing.config = signing.write("signing.toml", &signing.public_key);
        signing
    }

    /// A user config file `name` stating this policy with `key`, its tables
    /// after `preamble` and named under `table` (`signing`, or
    /// `--scope.signing` inside a `[[--scope]]` the preamble opens).
    fn write_with(
        &self,
        name: &str,
        (preamble, table): (&str, &str),
        key: &std::path::Path,
    ) -> std::path::PathBuf {
        let config = self.directory.join(name);
        std::fs::write(
            &config,
            format!(
                "{preamble}[{table}]\nbehavior = \"own\"\nbackend = \"ssh\"\nkey = '{}'\n\n\
                 [{table}.backends.ssh]\nprogram = '{}'\nallowed-signers = '{}'\n",
                key.display(),
                self.program.display(),
                self.allowed.display(),
            ),
        )
        .expect("write signing config");
        config
    }

    fn write(&self, name: &str, key: &std::path::Path) -> std::path::PathBuf {
        self.write_with(name, ("", "signing"), key)
    }

    /// How many signatures were made.
    fn signature_count(&self) -> usize {
        std::fs::read_to_string(&self.signatures).map_or(0, |log| log.lines().count())
    }
}

/// `jj` in the lab's work checkout under `jj_config`, which must succeed.
fn work_jj(lab: &Lab, jj_config: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("jj")
        .arg("--ignore-working-copy")
        .args(args)
        .current_dir(lab.work_path())
        .env("JJ_CONFIG", jj_config)
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("run jj");
    assert!(
        output.status.success(),
        "jj {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The signature of the commit `revision` names, as jj verifies it under
/// `jj_config`, or `unsigned`.
fn signature_of(lab: &Lab, jj_config: &std::path::Path, revision: &str) -> String {
    work_jj(
        lab,
        jj_config,
        &[
            "log",
            "--no-graph",
            "-r",
            revision,
            "-T",
            "if(signature, signature.status(), \"unsigned\")",
        ],
    )
}

fn release_signature(lab: &Lab, jj_config: &std::path::Path) -> String {
    signature_of(lab, jj_config, "release/2026-08-04")
}

#[test]
fn a_cut_is_signed_as_the_users_jj_would_sign_it() {
    // Given: a user jj config that signs the user's own commits with an ssh
    // key. A cut written unsigned under that policy shows Unverified, and
    // re-signing it by hand changes the id the audit knows the cut by.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let signing = SigningConfig::new(lab.temp_path());

    // When: knives cuts the release under that config.
    let output = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-04"],
    )
    .env("JJ_CONFIG", &signing.config)
    .output()
    .expect("run knives release cut");
    assert!(output.status.success(), "{output:?}");

    // Then: exactly one commit was signed — the published cut. The audit's
    // candidate is scratch that is dropped, so the jj CLI would never sign it,
    // and the audit reads each member by its own lines, which writes no commit.
    assert_eq!(signing.signature_count(), 1, "{output:?}");
    // And: the cut's signature verifies against that key.
    assert_eq!(release_signature(&lab, &signing.config), "good");
}

#[test]
fn a_cut_without_a_signing_policy_stays_unsigned() {
    // Given: no signing configuration anywhere (the lab's user config is empty).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: knives cuts the release.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(output.status.success(), "{output:?}");

    // Then: the cut is written as before, with no signature.
    assert_eq!(
        release_signature(&lab, std::path::Path::new("/dev/null")),
        "unsigned"
    );
}

#[test]
fn a_read_that_reconciles_divergent_operations_signs_what_it_rebases() {
    // Given: two jj operations made concurrently from the same parent — one
    // rewords feat/alpha, the other adds a child on top of it. Whichever
    // command loads the repository next merges them, rebasing the child onto
    // the reworded alpha and publishing it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let signing = SigningConfig::new(lab.temp_path());
    let unsigned = std::path::Path::new("/dev/null");
    let before = work_jj(
        &lab,
        unsigned,
        &["op", "log", "--no-graph", "-n", "1", "-T", "id"],
    );
    work_jj(
        &lab,
        unsigned,
        &["describe", "feat/alpha", "-m", "alpha reworded"],
    );
    work_jj(
        &lab,
        unsigned,
        &["--at-op", &before, "new", "feat/alpha", "-m", "child"],
    );

    // When: knives only reads (the release plan) under a signing policy.
    let plan = release_command(&lab, &home, ReleaseOutput::Text, &[])
        .env("JJ_CONFIG", &signing.config)
        .output()
        .expect("run knives release");
    assert_ne!(plan.status.code(), Some(3), "{plan:?}");

    // Then: the merge knives published signed the child it rebased, as the
    // jj CLI would have.
    assert_eq!(signing.signature_count(), 1, "{plan:?}");
    assert_eq!(
        signature_of(&lab, &signing.config, "description(exact:\"child\\n\")"),
        "good"
    );
}

#[test]
fn a_cut_is_written_with_the_identity_scoped_to_its_repository() {
    // Given: identity stated only in the user file, the way a per-project
    // identity usually is: a default, a `[[--scope]]` for this repository (by a
    // `~` path) that changes the email, and one for another path that changes
    // the name. The repository's own config states no identity.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let unsigned = std::path::Path::new("/dev/null");
    work_jj(&lab, unsigned, &["config", "unset", "--repo", "user.name"]);
    work_jj(&lab, unsigned, &["config", "unset", "--repo", "user.email"]);
    // jj matches the canonical repository path, so `~` must be canonical too.
    let user_home = lab.temp_path().canonicalize().expect("canonical lab dir");
    let config = lab.temp_path().join("scoped.toml");
    std::fs::write(
        &config,
        format!(
            "user.name = \"Default Name\"\n\
             user.email = \"default@example.test\"\n\n\
             [[--scope]]\n--when.repositories = [\"~/work\"]\n\
             [--scope.user]\nemail = \"scoped@example.test\"\n\n\
             [[--scope]]\n--when.repositories = ['{}']\n\
             [--scope.user]\nname = \"Elsewhere\"\n",
            user_home.join("second").display()
        ),
    )
    .expect("write scoped config");

    // When: knives cuts the release under that config.
    let output = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-04"],
    )
    .env("JJ_CONFIG", &config)
    .env("HOME", &user_home)
    .env_remove("JJ_USER")
    .env_remove("JJ_EMAIL")
    .output()
    .expect("run knives release cut");
    assert!(output.status.success(), "{output:?}");

    // Then: the cut carries this repository's scoped email and the default
    // name; the other path's scope did not apply.
    let author = work_jj(
        &lab,
        unsigned,
        &[
            "log",
            "--no-graph",
            "-r",
            "release/2026-08-04",
            "-T",
            "author.name() ++ \" <\" ++ author.email() ++ \">\"",
        ],
    );
    assert_eq!(author, "Default Name <scoped@example.test>");
}

/// The release commit's author email, read under no user config.
fn release_author_email(lab: &Lab) -> String {
    work_jj(
        lab,
        std::path::Path::new("/dev/null"),
        &[
            "log",
            "--no-graph",
            "-r",
            "release/2026-08-04",
            "-T",
            "author.email()",
        ],
    )
}

#[test]
fn a_later_jj_config_path_overrides_an_earlier_one() {
    // Given: identity stated only in two `JJ_CONFIG` files, both setting
    // `user.email`. jj loads the paths in order, so the later one wins.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let unsigned = std::path::Path::new("/dev/null");
    work_jj(&lab, unsigned, &["config", "unset", "--repo", "user.name"]);
    work_jj(&lab, unsigned, &["config", "unset", "--repo", "user.email"]);
    let first = lab.temp_path().join("first.toml");
    let second = lab.temp_path().join("second.toml");
    std::fs::write(
        &first,
        "user.name = \"First\"\nuser.email = \"first@example.test\"\n",
    )
    .expect("write first config");
    std::fs::write(&second, "user.email = \"second@example.test\"\n").expect("write second config");
    let paths = std::env::join_paths([&first, &second]).expect("join JJ_CONFIG");

    // When: knives cuts the release under `JJ_CONFIG=first:second`.
    let output = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-04"],
    )
    .env("JJ_CONFIG", &paths)
    .env_remove("JJ_USER")
    .env_remove("JJ_EMAIL")
    .output()
    .expect("run knives release cut");
    assert!(output.status.success(), "{output:?}");

    // Then: the cut carries the second file's email.
    assert_eq!(release_author_email(&lab), "second@example.test");
}

#[test]
fn a_cut_is_signed_by_a_signing_policy_scoped_to_the_environment() {
    // Given: the signing policy stated only inside a `[[--scope]]` that holds
    // when an environment variable is set — a condition jj judges for every
    // command, knives' own in-process writes included.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let signing = SigningConfig::new(lab.temp_path());
    let scoped = signing.write_with(
        "scoped-signing.toml",
        (
            "[[--scope]]\n--when.environments = [\"KNIVES_TEST_SIGNING\"]\n",
            "--scope.signing",
        ),
        &signing.public_key,
    );

    // When: knives cuts the release with that variable set.
    let output = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-04"],
    )
    .env("JJ_CONFIG", &scoped)
    .env("KNIVES_TEST_SIGNING", "1")
    .output()
    .expect("run knives release cut");
    assert!(output.status.success(), "{output:?}");

    // Then: the scope applied, and the cut is signed with its key.
    assert_eq!(signing.signature_count(), 1, "{output:?}");
    assert_eq!(release_signature(&lab, &signing.config), "good");
}

#[test]
fn a_cut_whose_signing_key_cannot_be_reached_fails_and_publishes_nothing() {
    // Given: a signing policy whose key file does not exist. Writing the cut
    // unsigned would be the silent failure this policy exists to prevent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let signing = SigningConfig::new(lab.temp_path());
    let broken = signing.write("broken.toml", &lab.temp_path().join("missing-key.pub"));
    let operations_before = operation_ids(&lab.work);

    // When: knives cuts the release under that policy.
    let output = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-04"],
    )
    .env("JJ_CONFIG", &broken)
    .output()
    .expect("run knives release cut");

    // Then: the cut fails, and nothing reached the repository: no new
    // operation, and no release bookmark.
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(operation_ids(&lab.work), operations_before, "{output:?}");
    assert_eq!(
        work_jj(
            &lab,
            std::path::Path::new("/dev/null"),
            &[
                "log",
                "--no-graph",
                "-r",
                "present(release/2026-08-04)",
                "-T",
                "commit_id"
            ],
        ),
        "",
        "{output:?}"
    );
}

#[test]
fn a_release_plan_reads_nothing_about_who_is_asking() {
    // Given: a state file nobody can read. Resolving who is acting reads it (a
    // terminal user inside a fork is named by the fork's claims); a plan writes
    // nothing, so it must never ask.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::write(home.path().join("state.json"), "{not json").expect("corrupt state");

    // When: the plan is asked for by an anonymous terminal user.
    let output = release_command(&lab, &home, ReleaseOutput::Text, &[])
        .output()
        .expect("run knives release");
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);

    // Then: the plan renders (a first cut has nothing to repair yet); the
    // unreadable state is nobody's problem here.
    assert!(
        text.contains("the first cut has nothing to repair"),
        "stdout: {text}\nstderr: {errors}"
    );
    assert!(!errors.contains("state.json"), "stderr: {errors}");
    assert_ne!(
        output.status.code(),
        Some(3),
        "stdout: {text}\nstderr: {errors}"
    );

    // And: a cut, which writes the ledger in the asker's name, is stopped by it.
    let cut = release_command(
        &lab,
        &home,
        ReleaseOutput::Text,
        &["cut", "release/2026-08-05"],
    )
    .output()
    .expect("run knives release cut");
    let errors = String::from_utf8_lossy(&cut.stderr);
    assert_eq!(cut.status.code(), Some(3), "stderr: {errors}");
    assert!(errors.contains("state.json"), "stderr: {errors}");
}

#[test]
fn a_cut_whose_record_fails_after_its_write_names_a_heading_held_twice_once() {
    // Given: alpha; beta, whose changelog's one `[Unreleased]` section holds
    // `### Added` twice; and a state file nobody can read, which the cut
    // reads to name who records its event, after it wrote the release.
    let lab = lab::Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch(
        "feat/beta",
        "CHANGELOG.md",
        "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Beta's addition.\n\n### Added\n\n\
         - An earlier addition.\n",
    );
    let (home, _consumer) = release_test_home(&lab);
    std::fs::write(home.path().join("state.json"), "{not json").expect("corrupt state");

    // When: an anonymous terminal user cuts the release.
    let output = release_command(&lab, &home, ReleaseOutput::Text, &["cut", release])
        .output()
        .expect("run knives release cut");

    // Then: the command exits with the record's failure, the release is the
    // cut's write, and one line names the file and the heading, for it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stderr.contains("state.json"), "{stderr}");
    let parents = release_parents(&lab, release);
    assert_eq!(parents.len(), 2, "{stdout}");
    assert!(parents.contains(&commit_at(&lab, "feat/beta")), "{stdout}");
    assert_eq!(
        stdout.matches("[Unreleased] holds").count(),
        1,
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(
            "  CHANGELOG.md: [Unreleased] holds `### Added` twice; knives changed no line for \
             it; join them by hand before you push\n"
        ),
        "{stdout}"
    );
}

#[test]
fn the_base_parent_is_not_stale_when_a_member_rebases_onto_the_advanced_trunk() {
    // Given: a release whose first parent is the bookmarkless shared base, and
    // one member rebased past it onto the advanced upstream — legitimate
    // upkeep, not a defect: a PR branch is expected to track the trunk it
    // will land on.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_origin_branch("main", "origin advance\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the bookmarkless base is not reported as a stale parent.
    assert!(
        !text.contains("carries no bookmark"),
        "base parent misread as stale: {text}"
    );
    // And: rebasing a member onto the advanced trunk is not itself a finding:
    // no per-branch `!! branch …` line names either member.
    assert!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with("!! branch ")),
        "a rebased member was reported as a per-branch finding: {text}"
    );
}

#[test]
fn older_upstream_release_parent_is_reported_as_a_superseded_base() {
    // Given: a release that accumulated the old and newer upstream-trunk positions.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let base0 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.advance_upstream("upstream advance\n");
    let base1 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.jj_work([
        "new",
        "-r",
        base0.trim(),
        "-r",
        base1.trim(),
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan is rendered.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: the obsolete trunk parent is explicitly classified for repair.
    assert!(
        text.contains("older upstream base superseded by"),
        "superseded base not reported: {text}"
    );
}

#[test]
fn a_cut_is_flat_and_carries_its_provenance() {
    // A release must be a flat merge of exactly the parents intended. The
    // failure this guards is silent: a cut that dropped a parent looks exactly
    // like one that did not, until work goes missing downstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha, "pull/10/head".to_owned()),
            (beta, "feat/beta".to_owned()),
        ],
    };
    let created =
        knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated).expect("cut");

    // Flat: exactly two parents, no nested integration node.
    let parents = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .parents_of(created.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2, "a release must be flat");

    // The dated name points at it, and the provenance rode along.
    let named = lab.revision(&lab.work, "release/2026-07-30", "commit_id");
    assert_eq!(named.trim(), created.as_str());
    let message = lab.revision(&lab.work, created.as_str(), "description");
    assert!(
        message.contains("from pull/10/head"),
        "provenance was lost: {message}"
    );
}

#[test]
fn a_cut_refuses_when_the_merge_did_not_get_the_parents_it_asked_for() {
    // The refusal was untested: the whole ensure! block could be deleted and the
    // flatness test still passed. jj dedupes duplicate parents, so asking for
    // the same commit twice produces a merge with fewer parents than requested,
    // which is exactly the "a branch's work was dropped" shape.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), alpha.clone()],
        provenance: vec![(alpha, "feat/alpha".to_owned())],
    };
    let outcome = knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated);
    assert!(
        outcome.is_err(),
        "a parent-count mismatch must refuse, got {outcome:?}"
    );

    // And the dated name was not set on a bad cut. Asked of the bookmark list,
    // because resolving a bookmark that rightly does not exist is an error.
    let names = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !names
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-07-30"),
        "a refused cut still claimed the name"
    );
}

#[test]
fn fixed_previous_position_keeps_the_published_remote_after_a_local_cut() {
    // Given: a fixed integration cut published to origin, then advanced only locally.
    let lab = lab::Lab::new();
    lab.branch("integration", "integration.txt", "published\n");
    lab.push_branch("integration");
    let published = Repo::open(&lab.work)
        .expect("open published repo")
        .resolve_commit("integration@origin")
        .expect("published integration tip");
    lab.jj_work(["new", "-r", "integration", "-m", "local integration cut"]);
    std::fs::write(lab.work.join("integration.txt"), "local cut\n").expect("write local cut");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = RepoEntry {
        release_branch: Some("integration".to_owned()),
        ..RepoEntry::new(
            lab.upstream.display().to_string(),
            lab.work.display().to_string(),
        )
    };
    let repo = Repo::open(&lab.work).expect("open after local cut");
    let local = repo
        .resolve_commit("integration")
        .expect("local integration tip");

    // When: the previous fixed release position is read after the local cut.
    let previous = knives::commands::release::previous_position(&repo, &entry);

    // Then: it is the unchanged published remote, not the new local cut.
    assert_ne!(local, published);
    assert_eq!(previous, Some(("integration@origin".to_owned(), published)));
}

#[test]
fn a_fixed_release_branch_is_cut_in_place_and_its_previous_position_is_the_old_cut() {
    // Given: a fork with one feature branch and a fixed integration branch scheme.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();

    // When: the first fixed cut is made and pushed.
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.trunk_ref())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let first = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("first cut");
    lab.push_branch("integration");
    lab.fetch_work();

    // MANDATORY reopen: Repo::open reads state at call time, and the first handle
    // predates the push/fetch that made integration@origin available locally.
    let opened = Repo::open(lab.work_path()).expect("reopen after fetch");
    let previous = knives::commands::release::previous_position(&opened, &entry)
        .expect("a pushed cut is a previous position");

    // Then: the remote-tracking ref is the old cut before any subsequent push.
    assert_eq!(
        previous,
        ("integration@origin".to_owned(), first.clone()),
        "the old cut is the previous release"
    );

    lab.branch("feat/beta", "beta.txt", "two\n");
    let opened = Repo::open(lab.work_path()).expect("reopen for second cut");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches for second cut");
    let trunk = opened
        .resolve_commit(&entry.trunk_ref())
        .expect("upstream trunk for second cut");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let second = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("second fixed cut may move integration sideways");
    let opened = Repo::open(lab.work_path()).expect("reopen after second cut");

    assert_eq!(
        opened
            .resolve_commit("integration")
            .expect("integration tip"),
        second,
        "the fixed bookmark advances to the fresh flat merge"
    );
    assert_eq!(
        knives::commands::release::previous_position(&opened, &entry),
        Some(("integration@origin".to_owned(), first)),
        "the still-unpushed second cut keeps the first published cut as previous"
    );
}

#[test]
fn a_fixed_cut_with_a_published_unrecorded_composition_records_instead_of_refusing() {
    // Given: a fixed release cut whose first recorded composition is published.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!("{config}release_branch = \"integration\"\n"),
    )
    .expect("configure fixed release branch");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("integration");
    lab.fetch_work();
    let first_published = commit_at(&lab, "integration@origin");

    // When: a composition edit is pushed before a cut records it.
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{included:?}");
    let published = commit_at(&lab, "integration");
    assert_ne!(
        published, first_published,
        "include must move the composition"
    );
    lab.push_branch("integration");
    lab.fetch_work();
    assert_eq!(commit_at(&lab, "integration@origin"), published);

    // Then: cut records the published composition without moving its bookmark.
    let recorded = knives_release(&lab, &home, &["cut"]);
    let stdout = String::from_utf8_lossy(&recorded.stdout);
    assert!(
        recorded.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut; previous recorded cut integration was {}",
            published.short(),
            first_published.short()
        )),
        "{stdout}"
    );
    assert_eq!(commit_at(&lab, "integration"), published);
    assert_eq!(commit_at(&lab, "integration@origin"), published);
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    let cut = entries
        .iter()
        .rev()
        .find(|entry| {
            entry.subject.as_deref() == Some("integration")
                && entry.text.starts_with("cut integration as ")
        })
        .unwrap_or_else(|| panic!("no recorded fixed cut: {entries:?}"));
    assert_eq!(
        cut.evidence.first().map(String::as_str),
        Some(published.as_str()),
        "the record-only cut must anchor its evidence at the published composition"
    );
    let mut recorded_members: Vec<&str> = cut.evidence[1..].iter().map(String::as_str).collect();
    recorded_members.sort_unstable();
    let alpha = lab.revision(&lab.work, "feat/alpha", "commit_id");
    let beta = lab.revision(&lab.work, "feat/beta", "commit_id");
    let mut published_members = [alpha.trim(), beta.trim()];
    published_members.sort_unstable();
    assert_eq!(
        recorded_members, published_members,
        "the record-only cut must record the members the published composition carries"
    );

    // And: once recorded, the identical published composition keeps the usual refusal.
    let repeated = knives_release(&lab, &home, &["cut"]);
    let repeated_stdout = String::from_utf8_lossy(&repeated.stdout);
    assert_eq!(
        repeated.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{repeated_stdout}"
    );
    assert!(
        repeated_stdout.contains(&format!(
            "demo: refusing to cut integration: identical to integration@origin ({}); nothing to cut\n",
            published.short()
        )),
        "{repeated_stdout}"
    );
}

#[test]
fn a_fixed_cut_records_a_published_composition_that_has_no_recorded_cut_yet() {
    // Given: a fixed release whose bookmark is published but whose ledger has
    // never recorded a cut (an adopted branch, or a ledger started fresh).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!("{config}release_branch = \"integration\"\n"),
    )
    .expect("configure fixed release branch");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("integration");
    lab.fetch_work();
    let published = commit_at(&lab, "integration@origin");
    let ledger_dir = home.path().join("ledger").join("demo");
    std::fs::remove_dir_all(&ledger_dir).expect("forget every recorded cut");

    // When: cut runs with local == origin and nothing recorded.
    let recorded = knives_release(&lab, &home, &["cut"]);
    let stdout = String::from_utf8_lossy(&recorded.stdout);

    // Then: the published composition is recorded as the first cut.
    assert!(
        recorded.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut; no previous recorded cut",
            published.short()
        )),
        "{stdout}"
    );
    assert_eq!(commit_at(&lab, "integration@origin"), published);
    let entries = knives::ledger::Ledger::at(ledger_dir)
        .read()
        .expect("read ledger")
        .entries;
    let cut = entries
        .iter()
        .rev()
        .find(|entry| {
            entry.subject.as_deref() == Some("integration")
                && entry.text.starts_with("cut integration as ")
        })
        .unwrap_or_else(|| panic!("no recorded fixed cut: {entries:?}"));
    assert_eq!(
        cut.evidence.first().map(String::as_str),
        Some(published.as_str())
    );
}

/// The release-test home with the fixed release branch `integration` and no
/// local consumer.
fn fixed_release_home(lab: &Lab) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!("{config}release_branch = \"integration\"\n"),
    )
    .expect("configure fixed release branch");
    home
}

/// `integration` composed outside knives as a compose script makes a
/// release: one merge whose first parent is the release in hand, then
/// `feat/alpha`, `feat/beta` and `feat/gamma`, four parents in all, not
/// pushed. Returns its commit.
fn compose_by_hand(lab: &Lab) -> knives::ids::CommitId {
    lab.jj_work([
        "new",
        "-r",
        "integration",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/gamma",
        "-m",
        "integration, composed by hand",
    ]);
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    assert_eq!(release_parents(lab, "integration").len(), 4);
    commit_at(lab, "integration")
}

/// Push `integration` and fetch it back. Returns the publish remote's
/// commit of it, which is the local one.
fn publish_integration(lab: &Lab) -> knives::ids::CommitId {
    lab.push_branch("integration");
    lab.fetch_work();
    let published = commit_at(lab, "integration@origin");
    assert_eq!(commit_at(lab, "integration"), published);
    published
}

/// [`compose_by_hand`], then [`publish_integration`].
fn publish_a_release_made_by_hand(lab: &Lab) -> knives::ids::CommitId {
    let composed = compose_by_hand(lab);
    let published = publish_integration(lab);
    assert_eq!(published, composed);
    published
}

/// The newest event the fork's ledger records about `integration`.
fn newest_integration_event(home: &tempfile::TempDir) -> knives::ledger::Entry {
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    entries
        .into_iter()
        .rev()
        .find(|entry| entry.subject.as_deref() == Some("integration"))
        .expect("an event of integration")
}

/// The release-drift findings of `knives --json audit demo --no-github`,
/// run against the lab's own origin with the same ledger; the registry is
/// put back afterwards.
fn release_drifts(lab: &Lab, home: &tempfile::TempDir) -> Vec<serde_json::Value> {
    let registry = home.path().join("repos.toml");
    let configured = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        configured.replace(
            "origin = \"https://forge.invalid/acme/work.git\"",
            &format!("origin = \"{}\"", lab.temp_origin().display()),
        ),
    )
    .expect("point the registry at the lab's origin");
    let output = knives(lab, home, &["--json", "audit", "demo", "--no-github"]);
    std::fs::write(&registry, configured).expect("restore the registry");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("audit emits JSON ({error}): {}", both_streams(&output)));
    report["findings"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["kind"] == "release-drift")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_fixed_cut_records_the_published_merge_whose_first_parent_is_the_previous_cut() {
    // Given: a fixed release branch cut by knives and pushed, then the
    // branch composed by hand as one merge of four parents, the previous cut
    // first, and pushed: the shape a compose script publishes.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab.push_branch("integration");
    lab.fetch_work();
    let cut = commit_at(&lab, "integration");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let published = publish_a_release_made_by_hand(&lab);

    // When: a plain cut runs.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records the published commit, makes no commit, and moves no
    // bookmark.
    let out = both_streams(&output);
    assert_ne!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut; previous \
             recorded cut integration was {}",
            published.short(),
            cut.short()
        )),
        "{out}"
    );
    assert_eq!(commit_at(&lab, "integration"), published, "{out}");
    assert_eq!(commit_at(&lab, "integration@origin"), published);

    // And: its event names the published commit, its three members, and
    // the previous cut as a carrier, never a member.
    let event = newest_integration_event(&home);
    assert_eq!(event.evidence.first(), Some(&published.as_str().to_owned()));
    let mut members: Vec<&str> = event.evidence[1..].iter().map(String::as_str).collect();
    members.sort_unstable();
    let mut wanted = ["feat/alpha", "feat/beta", "feat/gamma"]
        .map(|branch| lab.revision(lab.work_path(), branch, "commit_id"));
    wanted.sort_unstable();
    assert_eq!(
        members,
        wanted
            .iter()
            .map(|commit| commit.trim())
            .collect::<Vec<_>>()
    );
    assert!(
        event
            .parents
            .iter()
            .any(|parent| parent.commit == cut.as_str()
                && parent.branches == ["integration earlier write (carrier, not a member)"]),
        "{event:?}"
    );
    assert!(
        event.text.contains(&format!(
            "; carries integration's earlier write(s), not members: {}",
            cut.short()
        )),
        "{}",
        event.text
    );

    // And: once recorded, the same cut has nothing to do.
    let repeated = knives_release(&lab, &home, &["cut"]);
    let repeated_out = both_streams(&repeated);
    assert_eq!(repeated.status.code(), Some(3), "{repeated_out}");
    assert!(
        repeated_out.contains(&format!(
            "demo: refusing to cut integration: identical to integration@origin ({}); nothing to \
             cut",
            published.short()
        )),
        "{repeated_out}"
    );
}

#[test]
fn a_fixed_cut_of_a_published_commit_edited_by_hand_records_it_unchecked_and_makes_nothing() {
    // Given: a fixed release branch cut by knives and pushed, then the branch
    // composed by hand as one merge with `alpha.txt` edited in the merge
    // itself, and pushed. A cut of the composition in hand would make that
    // tree on those parents, its earlier write left out.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose_by_hand(&lab);
    lab.jj_work(["edit", "integration"]);
    std::fs::write(
        lab.work_path().join("alpha.txt"),
        "alpha, edited in the merge\n",
    )
    .expect("edit the merge");
    lab.jj_work(["new", "-r", "main@origin"]);
    let published = publish_integration(&lab);
    let ledger = || {
        knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
            .read()
            .expect("read ledger")
            .entries
            .len()
    };
    let before = ledger();

    // When: a plain cut runs.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records the published commit, says knives could not check
    // it against its recorded writes (the event carries the unchecked mark),
    // writes one event, makes no commit, and leaves the bookmark on it.
    let out = both_streams(&output);
    assert_ne!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut",
            published.short()
        )),
        "{out}"
    );
    assert!(out.contains("knives could not check integration"), "{out}");
    assert_eq!(ledger(), before + 1, "{out}");
    assert_eq!(commit_at(&lab, "integration"), published, "{out}");
}

/// What [`unpushed_cut_lab`] makes: the lab and home, the first cut, the cut
/// the publish remote never held, and the published commit.
struct UnpushedCut {
    lab: Lab,
    home: tempfile::TempDir,
    cut: knives::ids::CommitId,
    wrong: knives::ids::CommitId,
    published: knives::ids::CommitId,
}

impl UnpushedCut {
    /// How many entries the fork's ledger holds.
    fn events(&self) -> usize {
        knives::ledger::Ledger::at(self.home.path().join("ledger").join("demo"))
            .read()
            .expect("read ledger")
            .entries
            .len()
    }
}

/// A fixed release branch cut by knives and pushed; then the branch composed
/// by hand of the previous cut, `feat/alpha`, `feat/beta` and `feat/gamma`;
/// then a cut of the three members alone that the publish remote's history
/// does not hold, as Cut8's dc16713e. A fixed branch's cut now records only
/// the publish remote's commit, so that cut is made by pushing a composition
/// afresh of the three members and recording it, then pushing the
/// composition by hand over it: the remote's history no longer holds it.
/// That cut is then abandoned. The ledger's newest write is the cut the
/// remote's history does not hold.
fn unpushed_cut_lab() -> UnpushedCut {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab.push_branch("integration");
    lab.fetch_work();
    let cut = commit_at(&lab, "integration");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let composed = compose_by_hand(&lab);
    // A bookmark keeps the composition out of the cut's orphan gate.
    lab.jj_work(["bookmark", "create", "keep", "-r", composed.as_str()]);
    let wrong = compose_integration(&lab, &["feat/alpha", "feat/beta", "feat/gamma"]);
    publish_integration(&lab);
    let recorded = knives_release(&lab, &home, &["cut"]);
    assert_ne!(
        recorded.status.code(),
        Some(3),
        "{}",
        both_streams(&recorded)
    );
    assert_eq!(
        newest_integration_event(&home).evidence.first(),
        Some(&wrong.as_str().to_owned()),
        "fixture: the fresh composition is the newest recorded write"
    );
    lab.jj_work(["bookmark", "delete", "keep"]);
    lab.jj_work([
        "bookmark",
        "set",
        "integration",
        "-r",
        composed.as_str(),
        "--allow-backwards",
    ]);
    let published = publish_integration(&lab);
    assert_eq!(published, composed);
    lab.jj_work(["abandon", wrong.as_str()]);
    assert!(
        !release_drifts(&lab, &home).is_empty(),
        "control: the audit reads the unpushed cut as the newest"
    );
    UnpushedCut {
        lab,
        home,
        cut,
        wrong,
        published,
    }
}

#[test]
fn a_fixed_cut_over_an_abandoned_write_never_pushed_is_refused_naming_corrects() {
    // Given: [`unpushed_cut_lab`].
    let state = unpushed_cut_lab();
    let (lab, home, before) = (&state.lab, &state.home, state.events());

    // When: a plain cut runs.
    let refused = knives_release(lab, home, &["cut"]);

    // Then: it is refused, naming the unpushed cut and `--corrects`, and
    // nothing is written or moved.
    let out = both_streams(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut integration: its newest write, the recorded cut {}, is \
             abandoned and not in integration@origin ({})'s history, so the publish remote never \
             held it; `knives release cut --corrects {}` withdraws it and records {} in its \
             place; nothing was written",
            state.wrong.short(),
            state.published.short(),
            state.wrong.short(),
            state.published.short()
        )),
        "{out}"
    );
    assert_eq!(state.events(), before, "{out}");
    assert_eq!(commit_at(lab, "integration"), state.published, "{out}");

    // When: --corrects names another write.
    let other = knives_release(lab, home, &["cut", "--corrects", state.cut.as_str()]);

    // Then: it is refused, naming the newest write, and nothing is written.
    let other_out = both_streams(&other);
    assert_eq!(other.status.code(), Some(3), "{other_out}");
    assert!(
        other_out.contains(&format!(
            "integration's newest write is the recorded cut {}, and --corrects withdraws only \
             that one",
            state.wrong.short()
        )),
        "{other_out}"
    );
    assert_eq!(state.events(), before, "{other_out}");
}

#[test]
fn a_fixed_cut_with_corrects_withdraws_the_write_never_pushed_and_records_the_published_commit() {
    // Given: [`unpushed_cut_lab`].
    let state = unpushed_cut_lab();
    let (lab, home, before) = (&state.lab, &state.home, state.events());
    let (wrong, published) = (&state.wrong, &state.published);

    // When: the cut runs with --corrects naming the unpushed cut.
    let corrected = knives_release(lab, home, &["cut", "--corrects", wrong.as_str()]);

    // Then: the ledger withdraws that cut, then records the published commit
    // after the cut it follows; the audit finds no drift; and the bookmark
    // stays.
    let out = both_streams(&corrected);
    assert_ne!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut; previous \
             recorded cut integration was {}",
            published.short(),
            state.cut.short()
        )),
        "{out}"
    );
    // And: the withdrawal's line is printed, saying the published commit
    // carries every member of the withdrawn cut.
    assert!(
        out.contains(&format!(
            "demo: withdrew integration's recorded write (a cut) {}: integration@origin never \
             held it, and the cut of {} records the published commit in its place; {} carries \
             every member of it\n",
            wrong.short(),
            published.short(),
            published.short()
        )),
        "{out}"
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    assert_eq!(entries.len(), before + 2, "{out}");
    let withdrawal = &entries[entries.len() - 2];
    assert!(
        withdrawal.text.starts_with(&format!(
            "withdrew integration's recorded write (a cut) {}: integration@origin never held it",
            wrong.short()
        )),
        "{}",
        withdrawal.text
    );
    assert_eq!(
        withdrawal.evidence,
        [wrong.as_str().to_owned(), published.as_str().to_owned()]
    );
    let recorded = &entries[entries.len() - 1];
    assert_eq!(
        recorded.evidence.first(),
        Some(&published.as_str().to_owned())
    );
    let drifts = release_drifts(lab, home);
    assert!(drifts.is_empty(), "{drifts:?}");
    assert_eq!(commit_at(lab, "integration"), *published);

    // And: the same correction again is refused, as the published commit is
    // the newest write now, and a plain cut has nothing to do.
    let again = knives_release(lab, home, &["cut", "--corrects", wrong.as_str()]);
    assert_eq!(again.status.code(), Some(3), "{}", both_streams(&again));
    let plain = knives_release(lab, home, &["cut"]);
    let plain_out = both_streams(&plain);
    assert_eq!(plain.status.code(), Some(3), "{plain_out}");
    assert!(plain_out.contains("nothing to cut"), "{plain_out}");
}

#[test]
fn a_fixed_cut_with_corrects_records_the_newer_of_two_unrecorded_releases_over_the_older() {
    // Given: [`unpushed_cut_lab`], whose published release P8 the ledger does
    // not record; then `feat/delta` added and the branch composed by hand
    // again, P9: a merge of P8, then `feat/alpha`, `feat/beta`, `feat/gamma`
    // and `feat/delta`, pushed. P8 and P9 are both published, and neither is
    // recorded; the ledger's newest write is the abandoned cut.
    let state = unpushed_cut_lab();
    let (lab, home, before) = (&state.lab, &state.home, state.events());
    let (p8, wrong) = (&state.published, &state.wrong);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab.jj_work([
        "new",
        "-r",
        "integration",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-r",
        "feat/gamma",
        "-r",
        "feat/delta",
        "-m",
        "integration, composed by hand again",
    ]);
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let p9 = publish_integration(lab);
    assert_eq!(release_parents(lab, "integration").first(), Some(p8));

    // When: the cut runs with --corrects naming the abandoned cut; then
    // again with --at naming P8; then a plain cut.
    let entries_before = ledger_len(home);
    let refused = knives_release(lab, home, &["cut", "--corrects", wrong.as_str()]);
    assert_refused_until_recorded(&refused, (home, entries_before), &p9, &[p8]);
    let older = knives_release(
        lab,
        home,
        &["cut", "--corrects", wrong.as_str(), "--at", p8.as_str()],
    );
    assert_ne!(older.status.code(), Some(3), "{}", both_streams(&older));
    let corrected = knives_release(lab, home, &["cut"]);

    // Then: the ledger withdraws that cut and records P8, then P9, with P8
    // as an earlier write it carries and the four branches as its members;
    // the audit finds no drift, and the bookmark stays on P9.
    let out = both_streams(&corrected);
    assert_ne!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut",
            p9.short()
        )),
        "{out}"
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    assert_eq!(entries.len(), before + 3, "{out}");
    let withdrawal = &entries[entries.len() - 3];
    assert_eq!(
        withdrawal.evidence,
        [wrong.as_str().to_owned(), p8.as_str().to_owned()]
    );
    assert_eq!(
        entries[entries.len() - 2].evidence.first(),
        Some(&p8.as_str().to_owned())
    );
    let recorded = &entries[entries.len() - 1];
    assert_eq!(recorded.evidence.first(), Some(&p9.as_str().to_owned()));
    assert!(
        recorded
            .parents
            .iter()
            .any(|parent| parent.commit == p8.as_str()
                && parent.branches == ["integration earlier write (carrier, not a member)"]),
        "{recorded:?}"
    );
    assert_eq!(recorded.evidence.len(), 5, "{recorded:?}");
    let drifts = release_drifts(lab, home);
    assert!(drifts.is_empty(), "{drifts:?}");
    assert_eq!(commit_at(lab, "integration"), p9);
}

/// A bookmark `name` at a merge of `parents`, the working copy off it.
/// Returns its commit.
fn merge_branch(lab: &Lab, name: &str, parents: &[&str]) -> knives::ids::CommitId {
    let mut args = vec!["new"];
    for parent in parents {
        args.extend(["-r", parent]);
    }
    let message = format!("{name}: a merge");
    args.extend(["-m", message.as_str()]);
    lab::jj_in(lab.work_path(), &args);
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    commit_at(lab, name)
}

/// A bookmark `name` at one commit on `base` that writes `file`, the working
/// copy off it. Returns its commit.
fn commit_on(lab: &Lab, name: &str, base: &str, file: &str) -> knives::ids::CommitId {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write the file");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    commit_at(lab, name)
}

/// `integration` moved to a merge of `parents` composed by hand, not pushed,
/// the working copy off it. Returns its commit.
fn compose_integration(lab: &Lab, parents: &[&str]) -> knives::ids::CommitId {
    let mut args = vec!["new"];
    for parent in parents {
        args.extend(["-r", parent]);
    }
    args.extend(["-m", "integration, composed by hand"]);
    lab::jj_in(lab.work_path(), &args);
    lab.jj_work([
        "bookmark",
        "set",
        "integration",
        "-r",
        "@",
        "--allow-backwards",
    ]);
    lab.jj_work(["new", "-r", "main@origin"]);
    commit_at(lab, "integration")
}

/// A fixed release branch cut by knives of `feat/alpha` and `feat/beta`,
/// and pushed. Returns the lab, its home and the cut.
fn first_fixed_cut() -> (Lab, tempfile::TempDir, knives::ids::CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = publish_integration(&lab);
    (lab, home, cut)
}

/// How many entries the fork's ledger holds.
fn ledger_len(home: &tempfile::TempDir) -> usize {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
        .len()
}

/// Whether `event` records `commit` as an earlier write it carries: a
/// parent with the carrier label.
fn records_carrier(event: &knives::ledger::Entry, commit: &knives::ids::CommitId) -> bool {
    event.parents.iter().any(|parent| {
        parent.commit == commit.as_str()
            && parent.branches == ["integration earlier write (carrier, not a member)"]
    })
}

/// Whether `event` records `commit` as a member: in its evidence after the
/// write, and no carrier.
fn records_member(event: &knives::ledger::Entry, commit: &knives::ids::CommitId) -> bool {
    event
        .evidence
        .iter()
        .skip(1)
        .any(|member| member == commit.as_str())
        && !records_carrier(event, commit)
}

/// Assert that `output`, a cut, recorded `published` with `carrier` as its
/// earlier write and each of `members` as a member.
fn assert_recorded_with(
    (output, home): (&std::process::Output, &tempfile::TempDir),
    published: &knives::ids::CommitId,
    carrier: &knives::ids::CommitId,
    members: &[&knives::ids::CommitId],
) {
    let out = both_streams(output);
    assert_ne!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: recorded published composition integration@origin ({}) as the cut",
            published.short()
        )),
        "{out}"
    );
    let event = newest_integration_event(home);
    assert_eq!(event.evidence.first(), Some(&published.as_str().to_owned()));
    assert!(records_carrier(&event, carrier), "{event:?}");
    for member in members {
        assert!(records_member(&event, member), "{member:?}: {event:?}");
    }
}

/// Assert that `output`, a cut of `published`, a commit made on a release
/// published on the newest recorded write and never recorded, refused to
/// read which parent is that release: it names each of `unknown` and the
/// record of that release first with `--at`, and the ledger still holds
/// `before` entries.
fn assert_refused_until_recorded(
    output: &std::process::Output,
    (home, before): (&tempfile::TempDir, usize),
    published: &knives::ids::CommitId,
    unknown: &[&knives::ids::CommitId],
) {
    let out = both_streams(output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "refusing to cut integration: no parent of {} is a write of integration the ledger \
             records",
            published.short()
        )),
        "{out}"
    );
    assert!(
        out.contains("record it first (`knives release cut --at <commit>`)"),
        "{out}"
    );
    for parent in unknown {
        assert!(
            out.contains(&format!(
                "{}, a parent of {}, holds integration's earlier write",
                parent.short(),
                published.short()
            )),
            "{out}"
        );
    }
    assert_eq!(ledger_len(home), before, "{out}");
}

/// `cut --at <commit>` on the fixed branch, which must record it.
fn record_at(lab: &Lab, home: &tempfile::TempDir, commit: &knives::ids::CommitId) {
    let output = knives_release(lab, home, &["cut", "--at", commit.as_str()]);
    assert_ne!(output.status.code(), Some(3), "{}", both_streams(&output));
    assert_eq!(
        newest_integration_event(home).evidence.first(),
        Some(&commit.as_str().to_owned())
    );
}

#[test]
fn a_fixed_cut_records_a_member_merged_into_the_previous_cut_as_a_member() {
    // Given: [`first_fixed_cut`]; `feat/delta`, a merge of `feat/x` into
    // that cut, as a branch merged into the release is; then the branch
    // composed by hand of the cut, `feat/gamma` and `feat/delta`, pushed.
    let (lab, home, cut) = first_fixed_cut();
    lab.branch("feat/x", "x.txt", "x\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let delta = merge_branch(&lab, "feat/delta", &["integration", "feat/x"]);
    compose_integration(&lab, &["integration", "feat/gamma", "feat/delta"]);
    let published = publish_integration(&lab);

    // When: a plain cut runs.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records the published commit with the cut, its first parent,
    // as its carrier, and `feat/gamma` and `feat/delta`, which descends from
    // the cut, as members.
    let gamma = commit_at(&lab, "feat/gamma");
    assert_recorded_with((&output, &home), &published, &cut, &[&gamma, &delta]);
}

#[test]
fn a_fixed_cut_records_members_built_on_releases_beside_the_unrecorded_release_they_follow() {
    // Given: [`first_fixed_cut`] W1; P8 composed by hand of W1 and
    // `feat/gamma`, pushed and not recorded; then P9, oh-my-pi's Cut9 shape:
    // a merge of P8, `feat/d1`, a merge of `feat/y` into P8, and `fix/d2`, a
    // commit on W1; pushed. Every parent of P9 descends from W1.
    let (lab, home, w1) = first_fixed_cut();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let p8 = compose_integration(&lab, &["integration", "feat/gamma"]);
    publish_integration(&lab);
    lab.branch("feat/y", "y.txt", "y\n");
    let d1 = merge_branch(&lab, "feat/d1", &[p8.as_str(), "feat/y"]);
    let d2 = commit_on(&lab, "fix/d2", w1.as_str(), "d2.txt");
    compose_integration(&lab, &[p8.as_str(), "feat/d1", "fix/d2"]);
    let p9 = publish_integration(&lab);

    // When: a plain cut runs; then P8 is recorded with --at, and the plain
    // cut runs again.
    let before = ledger_len(&home);
    let refused = knives_release(&lab, &home, &["cut"]);
    assert_refused_until_recorded(&refused, (&home, before), &p9, &[&p8, &d1, &d2]);
    record_at(&lab, &home, &p8);
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records P9 with P8, its first parent, as its carrier, and
    // both other parents as members.
    assert_recorded_with((&output, &home), &p9, &p8, &[&d1, &d2]);
}

#[test]
fn a_fixed_cut_records_a_landing_on_an_unrecorded_release_as_a_member_beside_it() {
    // Given: [`first_fixed_cut`] W1; P8 composed by hand of W1 and
    // `feat/gamma`, pushed and not recorded; then P9, a merge of P8 and
    // `fix/landing`, a branch whose base is P8; pushed.
    let (lab, home, _) = first_fixed_cut();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let p8 = compose_integration(&lab, &["integration", "feat/gamma"]);
    publish_integration(&lab);
    let landing = commit_on(&lab, "fix/landing", p8.as_str(), "landing.txt");
    compose_integration(&lab, &[p8.as_str(), "fix/landing"]);
    let p9 = publish_integration(&lab);

    // When: a plain cut runs; then P8 is recorded with --at, and the plain
    // cut runs again.
    let before = ledger_len(&home);
    let refused = knives_release(&lab, &home, &["cut"]);
    assert_refused_until_recorded(&refused, (&home, before), &p9, &[&p8, &landing]);
    record_at(&lab, &home, &p8);
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records P9 with P8 as its carrier and the landing as a
    // member.
    assert_recorded_with((&output, &home), &p9, &p8, &[&landing]);
}

/// A fixed release branch cut of `feat/alpha` and pushed; `feat/beta`
/// included and pushed; `feat/gamma` included and not pushed, so that edit
/// stays visible; then the release's bookmark moved back to the pushed
/// edit. Returns the lab, its home, the pushed edit, the edit not pushed and
/// `feat/gamma`'s commit.
fn visible_unpushed_edit_lab() -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    publish_integration(&lab);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let published = publish_integration(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let unpushed = commit_at(&lab, "integration");
    lab.jj_work([
        "bookmark",
        "set",
        "integration",
        "-r",
        published.as_str(),
        "--allow-backwards",
    ]);
    assert!(
        lab::is_visible(&lab, &unpushed),
        "fixture: the edit stays visible"
    );
    let gamma = commit_at(&lab, "feat/gamma");
    (lab, home, published, unpushed, gamma)
}

#[test]
fn a_fixed_cut_with_corrects_refuses_a_newest_write_still_visible_and_writes_nothing() {
    // Given: [`visible_unpushed_edit_lab`].
    let (lab, home, published, unpushed, gamma) = visible_unpushed_edit_lab();
    let before = ledger_len(&home);

    // When: the cut runs with --corrects naming the visible edit.
    let corrected = knives_release(&lab, &home, &["cut", "--corrects", unpushed.as_str()]);

    // Then: it is refused, as the edit is not abandoned, with how to keep it
    // and how to withdraw it, and nothing is written.
    let out = both_streams(&corrected);
    assert_eq!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut integration --corrects {}: the recorded edit {} is visible, so \
             it is not abandoned, and a correction withdraws only a write nothing keeps: to keep \
             it, move integration to it (`jj bookmark set integration -r {} --allow-backwards`) \
             and push it; to withdraw it, abandon it first (`jj abandon {}`); nothing was written",
            unpushed.as_str(),
            unpushed.short(),
            unpushed.short(),
            unpushed.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_len(&home), before, "{out}");

    // When: it is abandoned, as the refusal says, and the cut runs again.
    lab.jj_work(["abandon", unpushed.as_str()]);
    let corrected = knives_release(&lab, &home, &["cut", "--corrects", unpushed.as_str()]);

    // Then: the withdrawal and the cut are written, and the withdrawal's
    // line names `feat/gamma`, which only the withdrawn edit held and the
    // published commit does not carry.
    let out = both_streams(&corrected);
    assert_ne!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: withdrew integration's recorded write (an edit) {}: integration@origin never \
             held it, and the cut of {} records the published commit in its place; {} does not \
             carry 1 member(s) of it: feat/gamma@{}\n",
            unpushed.short(),
            published.short(),
            published.short(),
            gamma.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_len(&home), before + 2, "{out}");
}

/// The fix-forward a refusal gives past `published`, the publish remote's
/// commit of `integration`, for `what` the subject lacks: a merge of
/// `published` and a branch that carries it, pushed, then the cut again.
fn fix_forward_text(published: &knives::ids::CommitId, what: &str) -> String {
    format!(
        "to carry {what}, make integration a merge of {p} and a branch that carries it (`jj new \
         {p} <branch> -m 'integration, fixed forward on {p}'`, `jj bookmark set integration -r \
         @`, then `jj new 'trunk()'`), push integration, then cut again, which records that \
         merge with {p} as its carrier",
        p = published.short()
    )
}

/// The clause a refusal gives after `nothing was recorded; ` where
/// `published`, the publish remote's commit of `integration`, holds commits
/// that `write`, its newest recorded write, lacks: record `published` as it
/// is, each drop named, then the fix-forward past it; never the move back
/// to the write.
fn record_then_fix_forward(
    published: &knives::ids::CommitId,
    write: &knives::ids::CommitId,
) -> String {
    format!(
        "integration@origin ({p}) holds commits that {w}, its newest recorded write, lacks, and \
         knives never moves integration behind its publish remote: re-run with --allow-drop \
         naming each drop below, which records {p} as it is and names each drop; then, {}",
        fix_forward_text(published, "what this cut lacks"),
        p = published.short(),
        w = write.short()
    )
}

/// Each value the `  name each drop: --allow-drop …` line of `out` gives,
/// as `--allow-drop VALUE` arguments for the run that states them.
fn named_drops(out: &str) -> Vec<String> {
    let line = out
        .lines()
        .find_map(|line| line.strip_prefix("  name each drop: "))
        .unwrap_or_else(|| panic!("no `name each drop:` line: {out}"));
    line.split("--allow-drop ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .flat_map(|value| ["--allow-drop".to_owned(), value.to_owned()])
        .collect()
}

/// A cut of `integration` with `drops` ([`named_drops`]) stated.
fn cut_dropping(lab: &Lab, home: &tempfile::TempDir, drops: &[String]) -> std::process::Output {
    let mut args = vec!["cut"];
    args.extend(drops.iter().map(String::as_str));
    knives_release(lab, home, &args)
}

#[test]
fn a_fixed_cut_lacking_a_fix_past_its_newest_write_records_it_then_names_a_fix_forward() {
    // Given: a fixed release branch cut of `feat/alpha` and `feat/beta`,
    // fixed by hand in place and adopted, the newest recorded write, and
    // pushed; then the branch published outside knives on that adopt, as
    // oh-my-pi's Cut9 is on its Cut7: a merge of the adopt and `feat/gamma`
    // whose own tree changes the fixed file, pushed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab::edit_by_hand(&lab, "integration", &[("fix.txt", "fixed by hand\n")]);
    lab::adopt(&lab, &home, "integration", "a fix by hand");
    let adopt = publish_integration(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.jj_work([
        "new",
        "-r",
        "integration",
        "-r",
        "feat/gamma",
        "-m",
        "integration, published by hand",
    ]);
    std::fs::write(lab.work_path().join("fix.txt"), "changed by hand\n").expect("write fix.txt");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let published = publish_integration(&lab);
    let before = ledger_len(&home);

    // When: a plain cut runs.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, naming the adopt's fix. Its remedy records the
    // published commit as it is, the drop named, then carries the fix past
    // it: never the move of `integration` back to the adopt, behind what
    // the publish remote holds, nor an edit by hand, which would rewrite the
    // published commit. Nothing is written, and no bookmark moves.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry the fix by hand of integration's \
             adopt {}, in 1 file(s): fix.txt\n  nothing was recorded; {}\n  name each drop: \
             --allow-drop fix.txt\n",
            adopt.short(),
            record_then_fix_forward(&published, &adopt)
        )),
        "{out}"
    );
    assert!(!out.contains("--allow-backwards"), "{out}");
    assert!(!out.contains("by hand to hold it"), "{out}");
    assert_eq!(ledger_len(&home), before, "{out}");
    assert_eq!(commit_at(&lab, "integration"), published);
    assert_eq!(commit_at(&lab, "integration@origin"), published);

    // When: the cut runs again with the drop it names.
    let recorded = cut_dropping(&lab, &home, &named_drops(&out));

    // Then: it records the published commit as it is: the adopt its carrier
    // and `feat/gamma` a member.
    let gamma = commit_at(&lab, "feat/gamma");
    assert_recorded_with((&recorded, &home), &published, &adopt, &[&gamma]);

    // When: the fix-forward is made as the refusal says, with a branch on
    // the published commit that puts the fix back, and pushed; then the cut
    // runs again.
    lab.jj_work(["new", "-r", published.as_str(), "-m", "fix/forward"]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["bookmark", "create", "fix/forward", "-r", "@"]);
    let message = format!("integration, fixed forward on {}", published.short());
    lab.jj_work([
        "new",
        "-r",
        published.as_str(),
        "-r",
        "fix/forward",
        "-m",
        message.as_str(),
    ]);
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let forward = publish_integration(&lab);
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it records the merge with the published commit, now the newest
    // recorded write, as its carrier and the branch as a member, and drops
    // nothing.
    let branch = commit_at(&lab, "fix/forward");
    assert_recorded_with((&output, &home), &forward, &published, &[&branch]);
    let out = both_streams(&output);
    assert!(!out.contains("--allow-drop"), "{out}");
}

/// [`visible_unpushed_edit_lab`], then `integration` published outside
/// knives on the pushed edit: a merge of it and `feat/delta`, pushed. The
/// edit not pushed, the newest recorded write, is beside that merge, not
/// under it. Returns the lab, its home, the published merge and the edit
/// not pushed.
fn published_beside_the_newest_write_lab() -> (
    Lab,
    tempfile::TempDir,
    knives::ids::CommitId,
    knives::ids::CommitId,
) {
    let (lab, home, pushed, unpushed, _gamma) = visible_unpushed_edit_lab();
    lab.branch("feat/delta", "delta.txt", "delta\n");
    compose_integration(&lab, &[pushed.as_str(), "feat/delta"]);
    let published = publish_integration(&lab);
    (lab, home, published, unpushed)
}

#[test]
fn a_fixed_cut_and_its_correction_never_move_the_branch_to_a_write_beside_its_published_commit() {
    // Given: [`published_beside_the_newest_write_lab`].
    let (lab, home, published, unpushed) = published_beside_the_newest_write_lab();
    let before = ledger_len(&home);

    // When: a plain cut runs, then the cut with --corrects naming the edit.
    let plain = knives_release(&lab, &home, &["cut"]);
    let corrected = knives_release(&lab, &home, &["cut", "--corrects", unpushed.as_str()]);

    // Then: both are refused, write nothing and move no bookmark. The plain
    // cut lacks `feat/gamma`, which only the edit holds, and names the
    // record of the published merge, then the fix-forward on it; the
    // correction names the edit's abandon and the correction again, which
    // records the published merge, then the fix-forward for what the edit
    // held. Neither names the move of `integration` to the edit, which would
    // leave the published merge behind.
    let plain_out = both_streams(&plain);
    assert_eq!(plain.status.code(), Some(3), "{plain_out}");
    assert!(
        plain_out.contains(&format!(
            "  nothing was recorded; {}\n  name each drop: --allow-drop {}\n",
            record_then_fix_forward(&published, &unpushed),
            commit_at(&lab, "feat/gamma").short()
        )),
        "{plain_out}"
    );
    let corrected_out = both_streams(&corrected);
    assert_eq!(corrected.status.code(), Some(3), "{corrected_out}");
    assert!(
        corrected_out.contains(&format!(
            "demo: refusing to cut integration --corrects {}: the recorded edit {u} is visible, \
             so it is not abandoned, and a correction withdraws only a write nothing keeps; \
             integration@origin ({p}) holds commits that it lacks, and knives never moves \
             integration behind its publish remote: abandon it (`jj abandon {u}`) and run this \
             again, which records {p} and names each member of it that {p} lacks; then, {}; \
             nothing was written",
            unpushed.as_str(),
            fix_forward_text(&published, "what it held"),
            u = unpushed.short(),
            p = published.short()
        )),
        "{corrected_out}"
    );
    for out in [&plain_out, &corrected_out] {
        assert!(!out.contains("--allow-backwards"), "{out}");
    }
    assert_eq!(ledger_len(&home), before);
    assert_eq!(commit_at(&lab, "integration"), published);
    assert_eq!(commit_at(&lab, "integration@origin"), published);
}

#[test]
fn a_fixed_cut_in_a_clone_without_the_newest_write_says_to_fetch_and_cut_again_never_to_move() {
    // Given: [`published_beside_the_newest_write_lab`] with `feat/gamma`
    // pushed, and the second clone fetched: it holds the published merge
    // and `feat/gamma`, and not the edit, so nothing there can tell whether
    // the edit holds the published merge.
    let (lab, home, published, unpushed) = published_beside_the_newest_write_lab();
    lab.push_branch("feat/gamma");
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &lab.second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&lab.second, ["git", "fetch", "--all-remotes"]);
    let before = ledger_len(&home);

    // When: a plain cut runs in the second clone.
    let output = lab::knives_command(
        &lab.second,
        home.path(),
        lab.temp_path(),
        &["--text", "release", "--repo", "demo", "cut"],
    )
    .output()
    .expect("run knives in the second clone");

    // Then: it is refused, naming the edit it cannot hold, with the fetch
    // and the cut again, never the move of `integration` to the edit; and
    // nothing is written.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "  nothing was recorded; fetch, then cut again: this repository does not hold {u}, \
             integration's newest recorded write, so it cannot tell whether that write holds \
             integration@origin ({p}), and knives never moves integration behind its publish \
             remote; if the fetch does not bring {u}, `knives release adopt integration` records \
             the commit this repository holds as integration's newest write, or re-run with \
             --allow-drop to state the drop is intended\n",
            u = unpushed.short(),
            p = published.short()
        )),
        "{out}"
    );
    assert!(!out.contains("--allow-backwards"), "{out}");
    assert_eq!(ledger_len(&home), before, "{out}");
}

#[test]
fn a_fixed_cut_beside_its_newest_write_records_the_published_merge_then_the_fix_forward_on_it() {
    // Given: [`published_beside_the_newest_write_lab`]: the published merge
    // is on an earlier write, beside the newest one, and lacks
    // `feat/gamma`, which only the newest write holds. The plain cut is
    // refused: record the published merge as it is, then the fix-forward.
    let (lab, home, published, unpushed) = published_beside_the_newest_write_lab();
    let refused = knives_release(&lab, &home, &["cut"]);
    let out = both_streams(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&record_then_fix_forward(&published, &unpushed)),
        "{out}"
    );

    // When: the cut runs again with the drop it names.
    let recorded = cut_dropping(&lab, &home, &named_drops(&out));

    // Then: it records the published merge as it is, the pushed edit its
    // carrier and `feat/delta` a member.
    let (pushed, delta) = (
        lab::release_parents(&lab, "integration")[0].clone(),
        commit_at(&lab, "feat/delta"),
    );
    assert_recorded_with((&recorded, &home), &published, &pushed, &[&delta]);

    // When: the fix-forward is made as the refusal prints it, with
    // `feat/gamma` as the branch, and pushed; then the cut runs again.
    let message = format!("integration, fixed forward on {}", published.short());
    lab.jj_work([
        "new",
        published.short(),
        "feat/gamma",
        "-m",
        message.as_str(),
    ]);
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "trunk()"]);
    let forward = publish_integration(&lab);
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: the push kept the published merge in the branch's history, and
    // the cut records the fix-forward with the published merge, its first
    // parent and now the newest recorded write, as its carrier and
    // `feat/gamma` as its member, dropping nothing.
    let opened = Repo::open(&lab.work).expect("open work checkout");
    assert!(
        opened
            .is_ancestor(&published, &forward)
            .expect("read ancestry"),
        "the push left the published merge behind"
    );
    let gamma = commit_at(&lab, "feat/gamma");
    assert_recorded_with((&output, &home), &forward, &published, &[&gamma]);
    let out = both_streams(&output);
    assert!(!out.contains("--allow-drop"), "{out}");
    assert!(
        !records_member(&newest_integration_event(&home), &published),
        "{out}"
    );
}

#[test]
fn a_recorded_write_with_an_empty_commit_never_brings_back_the_move_behind_the_published_commit() {
    // Given: [`published_beside_the_newest_write_lab`], and a peer's cut
    // event of `integration`, older than every write here, whose commit is
    // empty, as a peer's ledger can carry.
    let (lab, home, published, unpushed) = published_beside_the_newest_write_lab();
    let alpha = commit_at(&lab, "feat/alpha");
    std::fs::write(
        home.path()
            .join("ledger")
            .join("demo")
            .join("20260101T000000.000000000Z-0000.md"),
        format!(
            "+++\n\
             ts = \"2026-01-01T00:00:00Z\"\n\
             owner = \"a-peer\"\n\
             subject = \"integration\"\n\
             kind = \"event\"\n\
             evidence = [\"\", \"{}\"]\n\
             +++\n\
             cut integration as  with 1 parent(s): feat/alpha@{}\n",
            alpha.as_str(),
            alpha.short()
        ),
    )
    .expect("write a peer's cut event by hand");
    let before = ledger_len(&home);

    // When: a plain cut runs.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: the empty commit names no write, so the refusal still names the
    // record of the published merge and the fix-forward on it, never the
    // move of `integration` to the newest write; nothing is written.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "  nothing was recorded; {}\n",
            record_then_fix_forward(&published, &unpushed)
        )),
        "{out}"
    );
    assert!(!out.contains("--allow-backwards"), "{out}");
    assert_eq!(ledger_len(&home), before, "{out}");
}

#[test]
fn a_fixed_cut_with_corrects_refuses_a_write_the_publish_remote_holds_or_descends_from() {
    // Given: [`first_fixed_cut`]: the publish remote holds the newest
    // recorded write.
    let (lab, home, cut) = first_fixed_cut();
    let before = ledger_len(&home);
    let refusal = |published: &knives::ids::CommitId| {
        format!(
            "demo: refusing to cut integration --corrects {}: integration@origin ({}) holds that \
             write or a rewrite of it, so it was published; nothing was written",
            cut.as_str(),
            published.short()
        )
    };

    // When: the cut runs with --corrects naming that write.
    let held = knives_release(&lab, &home, &["cut", "--corrects", cut.as_str()]);

    // Then: it is refused as published, and nothing is written.
    let out = both_streams(&held);
    assert_eq!(held.status.code(), Some(3), "{out}");
    assert!(out.contains(&refusal(&cut)), "{out}");
    assert_eq!(ledger_len(&home), before, "{out}");

    // When: the branch is composed by hand on that write with `feat/gamma`
    // and pushed, so the publish remote descends from it, and the cut runs
    // with --corrects naming it again.
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose_integration(&lab, &["integration", "feat/gamma"]);
    let published = publish_integration(&lab);
    let descended = knives_release(&lab, &home, &["cut", "--corrects", cut.as_str()]);

    // Then: it is refused as published, and nothing is written.
    let out = both_streams(&descended);
    assert_eq!(descended.status.code(), Some(3), "{out}");
    assert!(out.contains(&refusal(&published)), "{out}");
    assert_eq!(ledger_len(&home), before, "{out}");
}

#[test]
fn a_fixed_cut_with_corrects_refuses_a_write_whose_rewrite_the_publish_remote_holds() {
    // Given: a fixed release branch cut by knives and not pushed, then
    // reworded (`jj describe`, the same change under a new commit) and the
    // rewrite pushed: the cut is hidden, and the publish remote holds its
    // rewrite.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = commit_at(&lab, "integration");
    lab.jj_work([
        "describe",
        "-r",
        "integration",
        "-m",
        "integration, reworded",
    ]);
    let published = publish_integration(&lab);
    assert_ne!(published, cut, "fixture: the rewrite is a new commit");
    assert!(!lab::is_visible(&lab, &cut), "fixture: the cut is hidden");
    let before = ledger_len(&home);

    // When: the cut runs with --corrects naming the cut.
    let corrected = knives_release(&lab, &home, &["cut", "--corrects", cut.as_str()]);

    // Then: it is refused as published, and nothing is written.
    let out = both_streams(&corrected);
    assert_eq!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut integration --corrects {}: integration@origin ({}) holds that \
             write or a rewrite of it, so it was published; nothing was written",
            cut.as_str(),
            published.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_len(&home), before, "{out}");
}

#[test]
fn a_fixed_cut_with_corrects_refuses_a_write_whose_rewrite_a_later_published_commit_holds() {
    // Given: a fixed release cut by knives and not pushed, reworded and
    // pushed, then a later published commit made on that rewrite. The
    // original cut is hidden, but the publish remote holds its rewrite in
    // history.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_release_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = commit_at(&lab, "integration");
    lab.jj_work([
        "describe",
        "-r",
        "integration",
        "-m",
        "integration, reworded",
    ]);
    let rewrite = publish_integration(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose_integration(&lab, &[rewrite.as_str(), "feat/gamma"]);
    let published = publish_integration(&lab);
    assert!(!lab::is_visible(&lab, &cut), "fixture: the cut is hidden");
    let before = ledger_len(&home);

    // When: the cut runs with --corrects naming the hidden original cut.
    let corrected = knives_release(&lab, &home, &["cut", "--corrects", cut.as_str()]);

    // Then: it is refused as published, and nothing is written.
    let out = both_streams(&corrected);
    assert_eq!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut integration --corrects {}: integration@origin ({}) holds that \
             write or a rewrite of it, so it was published; nothing was written",
            cut.as_str(),
            published.short()
        )),
        "{out}"
    );
    assert_eq!(ledger_len(&home), before, "{out}");
}

#[test]
fn a_fixed_cut_with_corrects_refuses_a_composition_in_hand_that_is_not_the_published_commit() {
    // Given: [`unpushed_cut_lab`], then `feat/delta` and the branch composed
    // by hand on the published commit, not pushed.
    let state = unpushed_cut_lab();
    let (lab, home, before) = (&state.lab, &state.home, state.events());
    lab.branch("feat/delta", "delta.txt", "delta\n");
    let local = compose_integration(lab, &["integration", "feat/delta"]);

    // When: the cut runs with --corrects naming the abandoned cut.
    let corrected = knives_release(lab, home, &["cut", "--corrects", state.wrong.as_str()]);

    // Then: it is refused, naming the move to the published commit; nothing
    // is written, and the bookmark stays.
    let out = both_streams(&corrected);
    assert_eq!(corrected.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut integration --corrects {}: the composition in hand is not \
             integration@origin ({}), the commit a correction records: `jj bookmark set \
             integration -r {} --allow-backwards` moves it there; nothing was written",
            state.wrong.as_str(),
            state.published.short(),
            state.published.short()
        )),
        "{out}"
    );
    assert_eq!(state.events(), before, "{out}");
    assert_eq!(commit_at(lab, "integration"), local);
}

#[test]
fn a_dated_cut_refuses_a_sideways_bookmark_move() {
    // Given: two unrelated flat dated cuts.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.branch("feat/beta", "beta.txt", "two\n");
    lab.octopus("release/2026-08-01", "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "three\n");
    lab.octopus("release/2026-08-02", "feat/alpha", "feat/gamma");
    let replacement = Repo::open(lab.work_path())
        .expect("open")
        .parents_of("release/2026-08-02")
        .expect("replacement dated cut parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect();

    // When: cut rebuilds the second merge under the first dated name.
    let moved = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "release/2026-08-01".to_owned(),
            parents: replacement,
            provenance: vec![],
        },
        &ReleaseScheme::Dated,
    );

    // Then: Dated routing retains jj's sideways-move protection.
    assert!(moved.is_err(), "dated cuts must not move sideways");
}

#[test]
fn plan_for_a_fixed_release_ignores_a_non_publish_remote() {
    // Given: the same fixed release exists on both the publish remote and upstream.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::release_model::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.trunk_ref())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("cut");
    lab.push_branch("integration");
    lab.jj_work(["bookmark", "track", "integration", "--remote", "upstream"]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "upstream",
        "--bookmark",
        "integration",
    ]);
    lab.fetch_work();
    let upstream = Repo::open(lab.work_path())
        .expect("reopen after fetch")
        .resolve_commit("integration@upstream");
    assert!(upstream.is_ok(), "upstream fixed release must be present");
    lab.jj_work(["bookmark", "delete", "integration"]);

    // When: planning selects the newest fixed release without a local bookmark.
    let forge = knives::forge::fake::FakeForge::default();
    let heads = knives::consumer_pins::ConsumerHeadMemo::default();
    let consumers = knives::commands::release::ConsumerInputs {
        slugs: &[],
        locals: &[],
        forge: &forge,
        cache_root: None,
        heads: &heads,
    };
    let plan =
        knives::commands::release::plan(&lab::lab_fork(&lab, "a-repo", &entry), &consumers, &[])
            .expect("plan");

    // Then: upstream cannot be mistaken for the publish remote's release.
    assert_eq!(plan.release.as_deref(), Some("integration@origin"));
}

#[test]
fn a_fresh_cut_carries_every_branch_and_nothing_else() {
    // What a dated release is: a flat merge of the current tip of everything we
    // carry. Not the trunk, not other releases.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-07-29", "feat/alpha", "feat/beta");

    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let carried = knives::release_model::carried_branches(&repo, "main", &ReleaseScheme::Dated)
        .expect("carried");
    let names: Vec<&str> = carried.iter().map(|(branch, _)| branch.as_str()).collect();

    assert!(names.contains(&"feat/alpha"));
    assert!(names.contains(&"feat/beta"));
    assert!(
        !names.iter().any(|n| n.starts_with("release/")),
        "a release is not a branch we carry"
    );
    assert!(
        !names.contains(&"main"),
        "the trunk is not a branch we carry"
    );
}

#[test]
fn cutting_a_release_reaps_the_superseded_one() {
    // Given: an existing cut and a consumer following it by branch, with the
    // work checkout's working copy moved off the cut (a working copy on it
    // keeps it: tests/release_reap.rs).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "main"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer release is cut through the binary.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the superseded cut is reaped while the newer one remains.
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work)
        .expect("reopen release repository")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04")
    );
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );
}

#[test]
fn a_cut_identical_to_the_published_previous_cut_is_refused() {
    // Given: a cut that origin already holds at exactly this tree.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.fetch_work();

    // When: nothing joined, advanced or moved, and a new name is asked for.
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is refused with the facts — the published cut and its
    // commit — and nothing else; nothing is created.
    let stdout = String::from_utf8_lossy(&second.stdout);
    let published = commit_at(&lab, "release/2026-08-04@origin");
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut release/2026-08-05: identical to release/2026-08-04@origin ({}); nothing to cut\n",
            published.short()
        )),
        "{stdout}"
    );
    assert_eq!(
        second.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{stdout}"
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );
}

/// A two-member `release/2026-08-04`, cut by hand, pushed to origin and fetched
/// back, with a consumer whose current pin is `origin_pin`. The consumer's
/// checkout content is never scanned (pins are read at its origin trunk), so
/// only `origin_pin` decides how the release is pinned.
fn published_release_pinned(origin_pin: &str) -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.push_branch("release/2026-08-04");
    lab.fetch_work();
    let (home, _consumer) =
        release_test_home_pinned(&lab, "branch = \"release/2026-08-03\"", origin_pin);
    (lab, home)
}

fn has_release(lab: &Lab, name: &str) -> bool {
    Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips")
        .keys()
        .any(|reference| reference.branch().as_str() == name)
}

#[test]
fn an_identical_cut_is_allowed_when_every_pin_of_the_previous_cut_is_frozen() {
    // Given: a published cut that a consumer pins by revision. The edit verbs
    // refuse to touch it (editing in place reaches nobody) and point at a new
    // dated cut; a verbatim cut under the new name is therefore the only way
    // to obtain an editable composition, and must not be refused as identical.
    let (lab, home) = published_release_pinned("rev = \"release/2026-08-04\"");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let refused = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert_eq!(refused.status.code(), Some(3), "{refused:?}");
    let published = commit_at(&lab, "release/2026-08-04@origin");
    let mut carried = release_parents(&lab, "release/2026-08-04");
    carried.sort();

    // When: the new dated name the refusal asked for is cut, verbatim.
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is named with the previous composition, says why an identical
    // composition was allowed, supersedes the pinned cut, and takes the edit
    // the old name refused.
    let stdout = String::from_utf8_lossy(&cut.stdout);
    assert_eq!(
        cut.status.code(),
        Some(i32::from(knives::cli::Exit::Ok.code())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "demo: release/2026-08-05 starts identical to release/2026-08-04@origin ({}); every pin of release/2026-08-04 is frozen, so this new name is the composition to edit — `include`, `advance`, `drop` or `rebase` it before pushing\n",
            published.short()
        )),
        "{stdout}"
    );
    let mut parents = release_parents(&lab, "release/2026-08-05");
    parents.sort();
    assert_eq!(parents, carried, "{stdout}");
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    assert!(!has_release(&lab, "release/2026-08-04"), "{stdout}");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    let included_stdout = String::from_utf8_lossy(&included.stdout);
    assert!(included.status.success(), "{included_stdout}");
    let mut parents = release_parents(&lab, "release/2026-08-05");
    parents.sort();
    carried.push(commit_at(&lab, "feat/gamma"));
    carried.sort();
    assert_eq!(parents, carried, "{included_stdout}");
}

#[test]
fn an_identical_cut_is_still_refused_when_a_consumer_follows_the_previous_cut() {
    // Given: the same published cut, but its consumer follows the branch
    // (`branch =`), so a repair reaches it in place and a verbatim cut under a
    // new name would ship nothing.
    let (lab, home) = published_release_pinned("branch = \"release/2026-08-04\"");
    let published = commit_at(&lab, "release/2026-08-04@origin");

    // When: a new name is asked for with nothing changed.
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: refused as identical, exactly as with no consumer at all.
    let stdout = String::from_utf8_lossy(&cut.stdout);
    assert_eq!(
        cut.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut release/2026-08-05: identical to release/2026-08-04@origin ({}); nothing to cut\n",
            published.short()
        )),
        "{stdout}"
    );
    assert!(!has_release(&lab, "release/2026-08-05"));
}

fn assert_refused_as_not_newest(output: &std::process::Output, name: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{name}: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut {name}: release/2026-08-04 is the newest release, so a dated cut takes a name that sorts after it (the reap after a cut keeps only the newest name)\n"
        )),
        "{name}: {stdout}"
    );
}

#[test]
fn a_dated_cut_refuses_a_name_that_does_not_sort_after_the_newest_release() {
    // Given: a published cut a consumer follows, then a third branch included
    // locally, so the composition in hand differs from anything published.
    let (lab, home) = published_release_pinned("branch = \"release/2026-08-04\"");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{included:?}");
    let edited = commit_at(&lab, "release/2026-08-04");

    for name in ["release/2026-08-04", "release/2026-08-03"] {
        // When: a name that does not sort after the newest release is asked for.
        let cut = knives_release(&lab, &home, &["cut", name]);

        // Then: refused before anything is built; the reap that follows a cut
        // would have taken the new name straight back.
        assert_refused_as_not_newest(&cut, name);
    }
    assert!(!has_release(&lab, "release/2026-08-03"));
    assert_eq!(commit_at(&lab, "release/2026-08-04"), edited);
}

#[test]
fn an_identical_cut_under_the_frozen_previous_cuts_own_or_an_older_name_is_refused() {
    // Given: the published cut every pin freezes — the state whose identical
    // cut is admitted under a newer name.
    let (lab, home) = published_release_pinned("rev = \"release/2026-08-04\"");
    let published = commit_at(&lab, "release/2026-08-04@origin");

    for name in ["release/2026-08-04", "release/2026-08-03"] {
        // When: the cut's own name or an older one is asked for, nothing changed.
        let cut = knives_release(&lab, &home, &["cut", name]);

        // Then: the name is refused before the frozen-pin exception can admit it.
        assert_refused_as_not_newest(&cut, name);
    }
    assert!(!has_release(&lab, "release/2026-08-03"));
    assert_eq!(commit_at(&lab, "release/2026-08-04"), published);
}

#[test]
fn a_cut_whose_composition_changed_since_publishing_is_named() {
    // Given: a published cut, then a third branch included locally.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    assert!(
        knives_release(&lab, &home, &["cut", "release/2026-08-04"])
            .status
            .success()
    );
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.fetch_work();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let include = knives_release(&lab, &home, &["include", "feat/gamma", "--why", "test"]);
    assert!(include.status.success(), "{include:?}");

    // When: a new name is asked for.
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the new name lands. The include moved local release/2026-08-04 past
    // its published copy, so the plan reports the two positions' trees differing
    // and the exit is Findings, not Ok; this test asserts only that the new name
    // landed and the identical-cut refusal did not fire.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(!stdout.contains("nothing to cut"), "{stdout}");
    assert_ne!(
        second.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{stdout}"
    );
}

#[test]
fn a_cut_whose_member_was_rewritten_with_the_same_content_is_a_new_composition() {
    // Given: a published cut whose member is then re-described — same tree,
    // new commit — and advanced onto the release. The candidate's tree equals
    // the published cut's; its parents do not.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    assert!(
        knives_release(&lab, &home, &["cut", "release/2026-08-04"])
            .status
            .success()
    );
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.fetch_work();
    let old_alpha = commit_at(&lab, "feat/alpha");
    lab.jj_work(["describe", "feat/alpha", "-m", "feat/alpha, reworded"]);
    let new_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(old_alpha, new_alpha, "describe must rewrite the member");
    let advance = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advance.status.success(), "{advance:?}");
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha) && !parents.contains(&old_alpha),
        "the release must hold the rewritten member: {parents:?}"
    );
    assert_eq!(
        knives::jj::changed_files_between(
            &lab.work,
            "release/2026-08-04@origin",
            "release/2026-08-04"
        )
        .expect("compare trees"),
        Vec::<String>::new(),
        "the rewrite must leave the tree unchanged, or this test proves nothing"
    );

    // When: a new name is asked for.
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: same tree, different parents: the cut lands rather than being refused.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(!stdout.contains("nothing to cut"), "{stdout}");
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&new_alpha),
        "the new cut must carry the rewritten member"
    );
}

#[test]
fn a_cut_identical_to_a_published_cut_known_only_remotely_is_refused() {
    // Given: the newest release is known only as `release/X@origin` (the local
    // bookmark forgotten after a push — tests/release_edit.rs does this), so
    // previous_release_for_cut names the remote ref.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    assert!(
        knives_release(&lab, &home, &["cut", "release/2026-08-04"])
            .status
            .success()
    );
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.fetch_work();

    // When: a new name is asked for.
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the comparison still finds the published copy, named once.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("identical to release/2026-08-04@origin ("),
        "{stdout}"
    );
    assert!(!stdout.contains("@origin@origin"), "{stdout}");
    assert_eq!(second.status.code(), Some(3), "{stdout}");
}

#[test]
fn a_fixed_scheme_cut_identical_to_its_published_position_is_refused() {
    // Given: a fixed release branch, cut and pushed, with nothing changed since.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = fixed_scheme_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first fixed cut")
            .resolve_commit("integration")
            .is_ok(),
        "first fixed cut was not named: {first:?}"
    );
    lab.push_branch("integration");
    lab.fetch_work();
    let published = commit_at(&lab, "integration@origin");

    // When: the fixed branch is cut again.
    let second = knives_release(&lab, &home, &["cut"]);

    // Then: refused as identical to the published position; the bookmark stays.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut integration: identical to integration@origin ({}); nothing to cut\n",
            published.short()
        )),
        "{stdout}"
    );
    assert_eq!(
        second.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{stdout}"
    );
    assert_eq!(commit_at(&lab, "integration"), published);
}

#[test]
fn a_fixed_scheme_cut_identical_to_its_published_position_is_refused_when_every_pin_is_frozen() {
    // Given: a fixed release branch, cut and pushed, that its only consumer
    // freezes by revision. A dated release in that state escapes through a new
    // dated name; a fixed branch has no other name to take, so the identical
    // cut stays refused and the remedy stays the edit verbs' fixed-scheme one.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_scheme_home_pinned(&lab, "rev = \"integration\"");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("integration");
    lab.fetch_work();
    let published = commit_at(&lab, "integration@origin");

    // When: the fixed branch is cut again with nothing changed.
    let second = knives_release(&lab, &home, &["cut"]);

    // Then: refused as identical; the bookmark stays where it was published.
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert_eq!(
        second.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut integration: identical to integration@origin ({}); nothing to cut\n",
            published.short()
        )),
        "{stdout}"
    );
    assert_eq!(commit_at(&lab, "integration"), published);
}

#[test]
fn a_named_cut_with_an_inconclusive_content_audit_returns_findings() {
    // Given: two members entangled in one file, so the cut is conflicted from birth
    // and neither member, read by its own lines, can be read in that file.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the cut is named despite its deliberately non-fatal audit result.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is retained, but automation receives the unresolved finding.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content check inconclusive"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Repo::open(&lab.work)
            .expect("open named cut")
            .resolve_commit("release/2026-08-05")
            .is_ok(),
        "the inconclusive cut was not named"
    );
}

#[test]
fn a_named_cut_that_drops_the_test_count_returns_findings() {
    // Given: one member reports ten tests while another makes the merged tree
    // report five. The check compares the cut against a single contributing
    // branch — the first parent — so the low-count member must not be first.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "branch-count", "5\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\ntest_count_command = \"if test -f branch-count; then cat branch-count; else printf 10; fi\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("configure test counter");

    // When: the real cut command observes the lower count in its new tree.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut remains named but reports the dropped-suite finding to automation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dropped that branch's tests"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_cut_is_one_operation_described_for_the_op_log() {
    // Given: branches ready for a first cut. A cut used to be two operations
    // (build the merge, then name it after the audit) with a crash window
    // between them that stranded an anonymous merge; the audit now reads a
    // candidate that was never committed, so pass = one operation and
    // fail = none (#18).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let operations_before = operation_ids(&lab.work);

    // When: the release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: creating AND naming the audited release is ONE operation.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "a cut must be one operation"
    );
    assert_eq!(
        newest_operation_description(&lab.work),
        "knives: cut release/2026-08-04"
    );
}

#[test]
fn a_discarded_candidate_leaves_no_trace() {
    // Given: a candidate cut that a failing audit would discard. The old flow
    // committed the merge before auditing, so a failure had to compensate with
    // an abandon — and a crash in between stranded an anonymous merge.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let operations_before = operation_ids(&lab.work);
    let visible_before = knives::jj::commits_matching(&lab.work, "all()").expect("list all");

    // When: the candidate is built, audit-shaped reads run against it, and it
    // is dropped without publishing.
    let candidate = knives::jj::candidate_release(
        &lab.work,
        knives::jj::CutSpec {
            source: None,
            parents: vec![alpha.clone(), beta],
            carriers: Vec::new(),
            message: "release: doomed candidate".to_owned(),
        },
    )
    .expect("build candidate");
    let conflicted = candidate.conflicted_files().expect("list conflicts");
    assert!(conflicted.is_empty(), "{conflicted:?}");
    let read = candidate
        .member_lines("feat/alpha", (&trunk, &alpha))
        .expect("read alpha's lines in the candidate");
    assert_eq!(
        read.reading,
        knives::own_lines::Reading::Carried,
        "alpha must be carried: {read:?}"
    );
    drop(candidate);

    // Then: no operation was written and no commit became visible.
    assert_eq!(operation_ids(&lab.work), operations_before);
    assert_eq!(
        knives::jj::commits_matching(&lab.work, "all()").expect("list all"),
        visible_before,
        "a discarded candidate leaked a commit"
    );
}

/// Registry home with `demo` on the fixed `integration` release branch.
fn fixed_scheme_registry(lab: &Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    home
}

/// [`fixed_scheme_registry`] plus an empty local consumer the release command
/// reads: nothing pins the fixed branch.
fn fixed_scheme_home(lab: &Lab) -> (tempfile::TempDir, tempfile::TempDir) {
    let home = fixed_scheme_registry(lab);
    let consumer = tempfile::tempdir().expect("create local consumer");
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.path().display().to_string(),
    )
    .expect("write local consumer fixture path");
    (home, consumer)
}

/// [`fixed_scheme_registry`] plus a local consumer whose current pin is `pin`
/// (`branch = "…"` follows the fixed branch, `rev = "…"` freezes on it).
fn fixed_scheme_home_pinned(lab: &Lab, pin: &str) -> tempfile::TempDir {
    let home = fixed_scheme_registry(lab);
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        &format!(
            "# checkout pin\nwork = {{ git = \"https://forge.invalid/acme/work.git\", {pin} }}\n"
        ),
        &format!(
            "# origin pin\nwork = {{ git = \"https://forge.invalid/acme/work.git\", {pin} }}\n"
        ),
    );
    std::fs::write(
        home.path().join("local-consumer"),
        consumer.display().to_string(),
    )
    .expect("write local consumer fixture path");
    home
}

#[test]
fn a_fixed_scheme_cut_carries_the_local_release_in_hand() {
    // Given: a fixed release branch, published, then edited locally. The cut
    // must name what is here — duplicating the stale published position would
    // silently revert the unpushed include.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = fixed_scheme_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first fixed cut")
            .resolve_commit("integration")
            .is_ok(),
        "first fixed cut was not named: {first:?}"
    );
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(
        String::from_utf8_lossy(&included.stdout).contains("included feat/beta"),
        "{included:?}"
    );
    let repo = Repo::open(&lab.work).expect("open after the include");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let edited = repo
        .resolve_commit("integration")
        .expect("locally edited release tip");

    // When: the fixed branch is cut again.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: the cut ran, and the composition in hand survived it: a fresh
    // commit whose parents are still both members. Its live predecessor differs
    // under the same fixed name, so reconciliation makes the completed cut a finding.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("cut integration as"), "{stdout}");
    let recut = commit_at(&lab, "integration");
    assert_ne!(recut, edited, "the fixed cut named nothing new: {stdout}");
    let parents = release_parents(&lab, "integration");
    assert!(
        parents.contains(&beta),
        "the cut reverted an unpushed include: {parents:?}\n{stdout}"
    );
    assert!(
        parents.contains(&alpha),
        "the cut lost a member it started with: {parents:?}\n{stdout}"
    );
}

/// The cut's line naming workspaces to forget, or `None` when it names none.
fn workspaces_to_forget(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find(|line| line.contains("belong to branches that no longer exist"))
        .map(str::to_owned)
}

#[test]
fn a_cut_never_names_a_workspace_a_live_claim_holds_or_knives_never_made() {
    // Given: a cut, an agent holding it through `knives start` (a claim and a
    // workspace named after the release), and a workspace knives never made.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let started = lab::knives_start(&lab, &home, "release/2026-08-04");
    assert!(started.status.success(), "{started:?}");
    let scratch = lab.temp_path().join("scratch-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "scratch",
        "--revision",
        "main@origin",
        scratch.to_str().expect("utf-8 path"),
    ]);

    // When: the next cut runs (its reap keeps the held release).
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it tells nobody to forget either workspace.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("cut release/2026-08-05 as "), "{stdout}");
    assert!(
        stdout.contains("  no workspace belongs to a branch that no longer exists\n"),
        "{stdout}"
    );
    assert_eq!(workspaces_to_forget(&stdout), None, "{stdout}");
    assert_eq!(output.status.code(), Some(1), "{stdout}");
}

#[test]
fn a_cut_never_names_a_workspace_only_a_live_claim_holds() {
    // Given: a branch an agent holds through `knives start` (a claim, a ledger
    // entry and workspace `feat-gamma`), whose bookmark is then deleted, so only
    // the claim says the workspace is in use.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let started = lab::knives_start(&lab, &home, "feat/gamma");
    assert!(started.status.success(), "{started:?}");
    lab.jj_work(["bookmark", "delete", "feat/gamma"]);

    // When: the next cut runs.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the held workspace is not named.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("  no workspace belongs to a branch that no longer exists\n"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(workspaces_to_forget(&stdout), None, "{stdout}");
}

#[test]
fn a_cut_names_the_workspace_of_a_recorded_branch_that_is_gone() {
    // Given: a branch knives recorded in the ledger, its workspace, then the
    // branch deleted, with no claim on it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let gamma = commit_at(&lab, "feat/gamma");
    let noted = lab::knives(
        &lab,
        &home,
        &[
            "notch",
            "feat/gamma",
            "-m",
            "abandoned experiment",
            "--evidence",
            gamma.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{noted:?}");
    let leftover = lab.temp_path().join("feat-gamma-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "feat-gamma",
        "--revision",
        "feat/gamma",
        leftover.to_str().expect("utf-8 path"),
    ]);
    lab.jj_work(["bookmark", "delete", "feat/gamma"]);

    // When: the next cut runs.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: that workspace, and only it, is named.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        workspaces_to_forget(&stdout).as_deref(),
        Some("  1 workspace(s) belong to branches that no longer exist: feat-gamma"),
        "{stdout}"
    );
}

#[test]
fn a_cut_refuses_to_drop_a_member_an_edit_added_after_the_previous_cut() {
    // Given: a cut of `feat/alpha`, then `feat/beta` added to it by
    // `include`, which the ledger records as an edit; then the release's
    // bookmark moved back to the cut's commit, as a machine that never
    // fetched the edit still has it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let beta = commit_at(&lab, "feat/beta");
    let included = knives_release(
        &lab,
        &home,
        &["include", "feat/beta", "--why", "added after the cut"],
    );
    assert!(
        included.status.success(),
        "{}",
        String::from_utf8_lossy(&included.stdout)
    );
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, naming the member the newest recorded write of
    // the release in hand holds and the cut does not carry, and nothing is
    // cut: the edit's member is held as a cut's members are.
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(out.contains(beta.short()), "{out}");
    assert!(
        Repo::open(&lab.work)
            .expect("open work checkout")
            .resolve_commit("release/2026-08-05")
            .is_err(),
        "{out}"
    );
}

/// Both streams of a knives run, for an assertion's message.
fn both_streams(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_cut_without_a_member_only_the_newest_write_holds_says_to_move_the_release_to_that_write() {
    // Given: a cut of `feat/alpha`, then `feat/beta` added to it by
    // `include`, an edit; then the release's bookmark moved back to the
    // cut's commit, as a machine that never fetched the edit still has it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let edit = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, and the refusal says to fetch and move the
    // release to the edit, naming its commit. It offers no `include`, which
    // on the older commit would write a second write of the release beside
    // the edit.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move release/2026-08-04 to {}, its newest \
             recorded write: `jj bookmark set release/2026-08-04 -r {} --allow-backwards`, or \
             re-run with --allow-drop to state the drop is intended\n",
            edit.short(),
            edit.short()
        )),
        "{out}"
    );
    assert!(!out.contains("restores a member"), "{out}");

    // When: the release is moved to the edit, as the refusal says, and the
    // next release is cut again.
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        edit.short(),
        "--allow-backwards",
    ]);
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is cut, carrying both members.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert_eq!(
        release_parents(&lab, "release/2026-08-05"),
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "{out}"
    );
}

#[test]
fn a_fixed_record_without_a_member_only_an_unpushed_edit_holds_says_to_move_the_release_to_it() {
    // Given: a fixed release cut of `feat/alpha` and pushed; `feat/beta`
    // included and pushed; `feat/gamma` included and not pushed; then the
    // release's bookmark moved back to the pushed edit, as a machine that
    // never fetched the newest edit still has it. The published composition
    // is no recorded cut, so a cut records it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!("{config}release_branch = \"integration\"\n"),
    )
    .expect("configure fixed release branch");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let published = commit_at(&lab, "integration");
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let newest = commit_at(&lab, "integration");
    lab.jj_work([
        "bookmark",
        "set",
        "integration",
        "-r",
        published.as_str(),
        "--allow-backwards",
    ]);

    // When: the release is cut, which records the published composition.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, naming `feat/gamma`, which only the newest edit
    // holds, and saying to fetch and move the release to that edit, naming
    // its commit; nothing is recorded.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry 1 member(s) which integration's \
             newest recorded write (an edit) holds:\n    feat/gamma@{}\n",
            commit_at(&lab, "feat/gamma").short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  nothing was recorded; fetch, then move integration to {}, its newest recorded \
             write: `jj bookmark set integration -r {} --allow-backwards`, or re-run with \
             --allow-drop to state the drop is intended\n",
            newest.short(),
            newest.short()
        )),
        "{out}"
    );
}

#[test]
fn a_cut_names_include_for_a_previous_cut_member_and_the_newest_write_for_one_only_it_holds() {
    // Given: a cut of `feat/alpha` and `feat/gamma`; `feat/gamma` dropped,
    // then `feat/beta` included, two edits; then the release's bookmark
    // moved back to the drop, as a machine that fetched the drop and not the
    // include still has it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/gamma", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{}", both_streams(&dropped));
    let after_drop = commit_at(&lab, "release/2026-08-04");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both_streams(&included));
    let newest = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        after_drop.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, naming `feat/gamma` with the previous cut and
    // `feat/beta` with the newest write, and each one's remedy: the move to
    // the newest write, named, for the member only that write holds, then
    // `include` for the previous cut's member.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: the previous cut release/2026-08-04 recorded 1 member(s) this \
             cut does not carry:\n    feat/gamma@{}\n",
            commit_at(&lab, "feat/gamma").short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry 1 member(s) which \
             release/2026-08-04's newest recorded write (an edit) holds:\n    feat/beta@{}\n",
            commit_at(&lab, "feat/beta").short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move release/2026-08-04 to {}, its newest \
             recorded write: `jj bookmark set release/2026-08-04 -r {} --allow-backwards`; \
             `knives release include <branch>` restores a member the previous cut recorded, or \
             re-run with --allow-drop to state the drop is intended\n",
            newest.short(),
            newest.short()
        )),
        "{out}"
    );

    // When: the remedy is followed in the order it is printed: the release
    // moved to its newest write, then `feat/gamma` included.
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        newest.as_str(),
        "--allow-backwards",
    ]);
    let restored = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: the include is written on the newest write, not beside it, so
    // the release holds all three members.
    assert!(restored.status.success(), "{}", both_streams(&restored));
    let members = ["feat/alpha", "feat/beta", "feat/gamma"].map(|branch| commit_at(&lab, branch));
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "{parents:?}");
    assert!(
        members.iter().all(|member| parents.contains(member)),
        "{parents:?}"
    );

    // When: the next release is cut again.
    let again = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it goes ahead, with the three members.
    assert!(again.status.success(), "{}", both_streams(&again));
    let cut = release_parents(&lab, "release/2026-08-05");
    assert!(members.iter().all(|member| cut.contains(member)), "{cut:?}");
}

#[test]
fn a_fixed_record_without_a_member_a_pushed_drop_removed_names_include_for_it() {
    // Given: a fixed release cut of `feat/alpha` and `feat/gamma` and
    // pushed; then `feat/gamma` dropped, an edit, and the drop pushed. The
    // published composition is no recorded cut, so a cut records it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");
    let registry = home.path().join("repos.toml");
    let config = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!("{config}release_branch = \"integration\"\n"),
    )
    .expect("configure fixed release branch");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    lab.push_branch("integration");
    lab.fetch_work();
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/gamma", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{}", both_streams(&dropped));
    lab.push_branch("integration");
    lab.fetch_work();

    // When: the release is cut, which records the published composition.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, naming `feat/gamma` with the previous cut and
    // `include` as its remedy; nothing is recorded.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: the previous cut integration recorded 1 member(s) this cut \
             does not carry:\n    feat/gamma@{}\n  nothing was recorded; `knives release include \
             <branch>` restores a member the previous cut recorded, or re-run with --allow-drop \
             to state the drop is intended\n",
            commit_at(&lab, "feat/gamma").short()
        )),
        "{out}"
    );
}

#[test]
fn a_cut_refuses_to_drop_a_fix_an_adopt_recorded_after_the_previous_cut() {
    // Given: a cut of `feat/alpha` and `feat/beta`, fixed by hand in place
    // (`jj squash` into it, which keeps its change id) and adopted, which the
    // ledger records as its newest write; then the release's bookmark back
    // at the cut's commit, as a machine that never fetched the adopt has it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.jj_work(["new", "-r", "release/2026-08-04"]);
    std::fs::write(lab.work_path().join("fix.txt"), "fixed by hand\n").expect("write fix.txt");
    lab.jj_work(["squash", "--into", "release/2026-08-04"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(
        &lab,
        &home,
        &["adopt", "release/2026-08-04", "--why", "a fix by hand"],
    );
    assert!(
        adopted.status.success(),
        "{}",
        String::from_utf8_lossy(&adopted.stdout)
    );
    let adopt = commit_at(&lab, "release/2026-08-04");
    assert_ne!(adopt, cut, "the squash rewrote the release");
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, naming the adopt's commit, and nothing is cut:
    // the cut would duplicate the commit the adopt replaced and drop the fix.
    let out = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(out.contains(adopt.short()), "{out}");
    assert!(
        Repo::open(&lab.work)
            .expect("open work checkout")
            .resolve_commit("release/2026-08-05")
            .is_err(),
        "{out}"
    );
}

/// A cut of `feat/alpha` and `feat/beta`, then `files` added by hand in
/// place (`jj squash` into the release, which keeps its change id) and
/// adopted, which the ledger records as the release's newest write; then
/// the release's bookmark back at the cut's commit, as a machine that never
/// fetched the adopt has it. Returns the adopt's commit.
fn release_behind_its_adopted_fix(
    files: &[&str],
) -> (Lab, tempfile::TempDir, knives::ids::CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.jj_work(["new", "-r", "release/2026-08-04"]);
    for file in files {
        std::fs::write(lab.work_path().join(file), "fixed by hand\n").expect("write the fix");
    }
    lab.jj_work(["squash", "--into", "release/2026-08-04"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(
        &lab,
        &home,
        &["adopt", "release/2026-08-04", "--why", "a fix by hand"],
    );
    assert!(adopted.status.success(), "{}", both_streams(&adopted));
    let adopt = commit_at(&lab, "release/2026-08-04");
    assert_ne!(adopt, cut, "the squash rewrote the release");
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);
    (lab, home, adopt)
}

#[test]
fn a_cut_without_a_fix_an_adopt_recorded_goes_ahead_only_where_allow_drop_names_each_file() {
    // Given: the release's bookmark back at the cut, behind the adopt of its
    // fix by hand at `also.txt` and `fix.txt`.
    let (lab, home, adopt) = release_behind_its_adopted_fix(&["also.txt", "fix.txt"]);
    let not_cut = || {
        Repo::open(&lab.work)
            .expect("open work checkout")
            .resolve_commit("release/2026-08-05")
            .is_err()
    };

    // When: the next release is cut with the drop of one file stated.
    let one = knives_release(
        &lab,
        &home,
        &["cut", "release/2026-08-05", "--allow-drop", "fix.txt"],
    );

    // Then: it is refused, naming the adopt and both files, and giving the
    // `--allow-drop` of each; nothing is cut.
    let out = both_streams(&one);
    assert_eq!(one.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry the fix by hand of \
             release/2026-08-04's adopt {}, in 2 file(s): also.txt, fix.txt\n",
            adopt.short()
        )),
        "{out}"
    );
    assert!(
        out.contains("  name each drop: --allow-drop also.txt --allow-drop fix.txt\n"),
        "{out}"
    );
    assert!(not_cut(), "{out}");

    // When: both drops are stated, and one more value that names no drop.
    let stray = knives_release(
        &lab,
        &home,
        &[
            "cut",
            "release/2026-08-05",
            "--allow-drop",
            "also.txt",
            "--allow-drop",
            "fix.txt",
            "--allow-drop",
            "nothing.txt",
        ],
    );

    // Then: it is refused for that value, no line says the cut goes without
    // a file, and nothing is cut.
    let stray_out = both_streams(&stray);
    assert_eq!(stray.status.code(), Some(3), "{stray_out}");
    assert!(
        stray_out.contains(
            "demo: refusing: --allow-drop nothing.txt names no drop of this write; nothing was \
             written\n"
        ),
        "{stray_out}"
    );
    assert!(
        !stray_out.contains("--allow-drop: cutting without"),
        "{stray_out}"
    );
    assert!(not_cut(), "{stray_out}");

    // When: each drop is stated, one file by the adopt and its path.
    let adopt_file = format!("{}:fix.txt", adopt.short());
    let both = knives_release(
        &lab,
        &home,
        &[
            "cut",
            "release/2026-08-05",
            "--allow-drop",
            "also.txt",
            "--allow-drop",
            &adopt_file,
        ],
    );

    // Then: it is cut without the fix, and both its line and its event name
    // the adopt, its release, its commit and both files.
    let both_out = both_streams(&both);
    assert_eq!(both.status.code(), Some(0), "{both_out}");
    assert!(
        both_out.contains(&format!(
            "demo: --allow-drop: cutting without the fix by hand of release/2026-08-04's adopt \
             {}, in 2 file(s): also.txt, fix.txt\n",
            adopt.short()
        )),
        "{both_out}"
    );
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    let event = entries
        .iter()
        .rev()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-05"))
        .unwrap_or_else(|| panic!("no cut event: {entries:?}"));
    assert!(
        event.text.contains(&format!(
            "; previous cut release/2026-08-04 recorded 2 member(s); all carried; without the fix \
             by hand of release/2026-08-04's adopt {}, in 2 file(s): also.txt, fix.txt",
            adopt.short()
        )),
        "{}",
        event.text
    );
}

#[test]
fn a_cut_without_a_conflict_an_adopt_resolved_says_to_move_the_release_to_it_and_goes_ahead_there()
{
    // Given: a cut of `feat/alpha` and `feat/beta`, which both write
    // `shared.txt`, so the cut leaves it conflicted; the conflict resolved by
    // hand in place (`jj edit`, which keeps the change id) and adopted; then
    // the release's bookmark back at the conflicted cut, as a machine that
    // never fetched the adopt has it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "{}",
        both_streams(&first)
    );
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work_path().join("shared.txt"), "alpha\nbeta\n").expect("resolve");
    lab.jj_work(["new", "-r", "main@origin"]);
    let adopted = knives_release(&lab, &home, &["adopt", "release/2026-08-04"]);
    assert!(adopted.status.success(), "{}", both_streams(&adopted));
    let adopt = commit_at(&lab, "release/2026-08-04");
    assert_ne!(adopt, cut, "the resolution rewrote the release");
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is refused, naming the adopt and the file it resolved by hand,
    // and the refusal says to fetch and move the release to the adopt, or
    // to keep the new content by an adopt of it.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "demo: refusing to cut: this cut does not carry the fix by hand of \
             release/2026-08-04's adopt {}, in 1 file(s): shared.txt\n",
            adopt.short()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  the candidate was discarded; fetch, then move release/2026-08-04 to {}, its newest \
             recorded write: `jj bookmark set release/2026-08-04 -r {} --allow-backwards`; to \
             keep the new content of those files, edit release/2026-08-04 by hand to hold it and \
             run `knives release adopt release/2026-08-04`, or re-run with --allow-drop to state \
             the drop is intended\n",
            adopt.short(),
            adopt.short()
        )),
        "{out}"
    );

    // When: the release is moved to the adopt, as the refusal says, and the
    // next release is cut again.
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "-r",
        adopt.as_str(),
        "--allow-backwards",
    ]);
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it is cut, and it carries the resolution.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert_eq!(
        Repo::open(&lab.work)
            .expect("open work checkout")
            .file_bytes(&commit_at(&lab, "release/2026-08-05"), "shared.txt")
            .expect("read shared.txt"),
        Some(b"alpha\nbeta\n".to_vec()),
        "{out}"
    );
}

#[test]
fn a_cut_refuses_to_drop_a_member_whose_older_version_it_carries() {
    // Given: a cut of `feat/alpha` and `feat/beta`; `feat/beta` then changed
    // in place (`jj squash` into it, which keeps its change id), so jj
    // rebases the cut onto it; the next release cut, which records the newer
    // `feat/beta`; then that release's bookmark at the first cut's commit,
    // whose parent is the older `feat/beta`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let cut = commit_at(&lab, "release/2026-08-04");
    let older = commit_at(&lab, "feat/beta");
    lab.jj_work(["new", "-r", "feat/beta"]);
    std::fs::write(lab.work_path().join("more.txt"), "more of beta\n").expect("write more.txt");
    lab.jj_work(["squash", "--into", "feat/beta"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    let newer = commit_at(&lab, "feat/beta");
    assert_ne!(newer, older, "the squash rewrote feat/beta");
    let second = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    assert!(second.status.success(), "{}", both_streams(&second));
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&newer),
        "{}",
        both_streams(&second)
    );
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-05",
        "-r",
        cut.as_str(),
        "--allow-backwards",
    ]);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-06"]);

    // Then: it is refused, naming the newer `feat/beta`, and nothing is cut:
    // the older version shares its change id, but it is what the newer one
    // replaced, and it lacks `more.txt`.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!("    feat/beta@{}\n", newer.short())),
        "{out}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("open work checkout")
            .resolve_commit("release/2026-08-06")
            .is_err(),
        "{out}"
    );
}

#[test]
fn a_cut_after_an_advance_to_a_member_rebased_in_another_clone_goes_ahead() {
    // Given: a cut of `feat/alpha` and `feat/beta`; `feat/alpha` pushed, then
    // rebased in a second clone onto an upstream trunk that writes its file
    // too, the conflict resolved there on alpha's lines, and pushed; fetched
    // here, where no operation records that rewrite; then `advance`, which
    // records the rebased `feat/alpha` as the member.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{}", both_streams(&first));
    let older = commit_at(&lab, "feat/alpha");
    lab.push_branch("feat/alpha");
    lab.upstream_trunk_file("alpha.txt", "trunk\n");
    let second = lab.second.clone();
    let upstream = lab.upstream.display().to_string();
    lab.jj_at(
        &second,
        ["git", "remote", "add", "upstream", upstream.as_str()],
    );
    lab.jj_at(&second, ["git", "fetch", "--all-remotes"]);
    lab.jj_at(
        &second,
        ["bookmark", "track", "feat/alpha", "--remote", "origin"],
    );
    lab.jj_at(
        &second,
        ["rebase", "-r", "feat/alpha", "-d", "main@upstream"],
    );
    lab.jj_at(&second, ["edit", "feat/alpha"]);
    std::fs::write(second.join("alpha.txt"), "trunk\nalpha\n").expect("resolve alpha.txt");
    lab.jj_at(&second, ["new"]);
    lab.jj_at(
        &second,
        [
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "feat/alpha",
        ],
    );
    lab.fetch_work();
    let rebased = commit_at(&lab, "feat/alpha");
    assert_ne!(rebased, older, "the second clone rebased feat/alpha");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{}", both_streams(&advanced));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it goes ahead with the rebased `feat/alpha`: the newest recorded
    // write names it, with the change id of the member the previous cut
    // recorded, so that older member is superseded and not held.
    let out = both_streams(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(
        release_parents(&lab, "release/2026-08-05").contains(&rebased),
        "{out}"
    );
}
