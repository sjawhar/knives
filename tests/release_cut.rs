//! `knives release cut`: a flat merge of every carried branch, and nothing else.
//!
//! The plan says when the release lags the trunk and which base is superseded.
//! The cut is one operation that carries its provenance, refuses a merge that
//! did not get the parents it asked for, audits members from their fork point,
//! returns findings for an inconclusive audit or a dropped test count, and
//! discards a failed candidate without trace. The fixed scheme cuts in place
//! and keeps its published position; a dated cut refuses a sideways move.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::config::RepoEntry;
use knives::detect::landed::RebaseOutcome;
use knives::ids::ReleaseScheme;
use knives::jj::Repo;
use lab::{
    Lab, ReleaseOutput, commit_at, knives_release, newest_operation_description, operation_ids,
    release_command, release_parents, release_test_home, release_test_home_pinned,
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
    // candidate and replays are scratch that is dropped, so the jj CLI would
    // never sign them; signing each would run the key once per replayed commit.
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
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("OMP_SESSION_ID")
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
    .env_remove("KNIVES_OWNER")
    .env_remove("CLAUDE_CODE_SESSION_ID")
    .env_remove("OMP_SESSION_ID")
    .output()
    .expect("run knives release cut");
    let errors = String::from_utf8_lossy(&cut.stderr);
    assert_eq!(cut.status.code(), Some(3), "stderr: {errors}");
    assert!(errors.contains("state.json"), "stderr: {errors}");
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
    // and every member's replay onto it answers nothing either way.
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
    let mut candidate = knives::jj::candidate_release(
        &lab.work,
        knives::jj::CutSpec {
            source: None,
            parents: vec![alpha.clone(), beta],
            message: "release: doomed candidate".to_owned(),
        },
    )
    .expect("build candidate");
    let conflicted = candidate.conflicted_files().expect("list conflicts");
    assert!(conflicted.is_empty(), "{conflicted:?}");
    let replay = candidate
        .replay_outcome(trunk.as_str(), alpha.as_str())
        .expect("replay alpha onto the candidate");
    assert_eq!(replay, RebaseOutcome::Empty, "alpha must be carried");
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
