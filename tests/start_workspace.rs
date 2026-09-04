//! Where `knives start` puts a workspace.
//!
//! A new branch starts on the release's shared base — the fetched upstream
//! trunk when no release exists — never on the current change and never on
//! upstream's newer tip, so composing it into the next cut forces no rebase. An
//! existing branch is continued from its tip; a divergent one is refused, with
//! its tips named, before any claim is recorded.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::BranchName;
use knives::jj::Repo;
use knives::store::{OwnerKind, Store};
use lab::{Lab, commit_at, knives_start, release_test_home, start_command};
use std::process::Command;

#[test]
fn a_new_workspace_is_based_on_the_upstream_trunk_not_the_current_change() {
    // The accident this default exists to prevent: an agent sitting in a release
    // workspace runs `jj new` and silently inherits the release merge as a
    // parent, so unrelated work rides into its pull request.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/dated", "feat/alpha", "feat/beta");
    // Move the upstream trunk so it differs from our fork's copy. Without this
    // the two are the same commit and the test passes whichever trunk the code
    // uses, proving nothing.
    lab.advance_upstream("moved on\n");

    // Given: the working copy is parked on the release merge, the dangerous spot
    let parked = lab.revision(&lab.work, "@", "change_id.short(8)");
    assert!(!parked.trim().is_empty());

    // When: a workspace is opened the way `knives start` opens one
    let destination = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &destination, "main@upstream")
        .expect("add workspace");

    // Then: its only parent is the upstream trunk, not the release merge
    let parents = lab.revision(&destination, "parents(@)", "commit_id.short(12) ++ \"\\n\"");
    let upstream = lab.revision(&lab.work, "main@upstream", "commit_id.short(12)");
    let listed: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(
        listed,
        vec![upstream.trim()],
        "new work must sit on the upstream trunk alone"
    );
}

#[test]
fn start_bases_a_new_branch_on_the_shared_base_not_the_advanced_upstream() {
    // Given: a release whose members fork from today's trunk, then upstream advances.
    // Basing new work on the advanced tip would drag that advance into the next
    // cut through one member — the mixed-base conflict storm (#10).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let repo = Repo::open(&lab.work).expect("open");
    let base_before_advance = repo.resolve_commit("main@origin").expect("resolve base");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = knives_start(&lab, &home, "feat/gamma");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the shared base, not the advanced tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        base_before_advance.as_str(),
        "based on {parent}, expected the shared base"
    );
}

#[test]
fn start_without_a_release_uses_the_fetched_upstream_trunk() {
    // Given: a registry with no release and an upstream tip distinct from origin.
    let lab = lab::Lab::new();
    lab.advance_upstream("upstream advance\n");
    let upstream_trunk = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");
    let (home, _consumer) = release_test_home(&lab);

    // When: the binary starts a branch without a release base to select.
    let output = knives_start(&lab, &home, "feat/no-release");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: its parent is the fetched upstream trunk and the fallback is disclosed.
    let workspace = lab.work.parent().expect("parent").join("feat-no-release");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(parent, upstream_trunk.as_str());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("(the fetched upstream trunk)"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn start_bases_a_new_branch_on_a_flat_releases_fork_point() {
    // Given: a doctrine-flat release — no trunk parent to find — and an
    // upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let fork_point = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = knives_start(&lab, &home, "feat/gamma");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the members' fork point, not the tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        fork_point.as_str(),
        "based on {parent}, expected the flat release's fork point"
    );
}

#[test]
fn start_on_an_existing_branch_continues_from_its_tip() {
    // Given: a release, an upstream advance, and a branch with work on it. An
    // agent claiming that branch wants its workspace on the work, not one
    // `jj new <branch>` away from it on the shared base.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);
    let tip = commit_at(&lab, "feat/alpha");

    let output = knives_start(&lab, &home, "feat/alpha");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "start failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-alpha");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        tip.as_str(),
        "the workspace must sit on the branch tip"
    );
    assert!(stdout.contains("(feat/alpha's tip)"), "{stdout}");
}

#[test]
fn start_on_a_name_that_exists_only_upstream_starts_a_new_branch_on_the_shared_base() {
    // Given: a release whose members fork from today's trunk; upstream advances
    // and grows a branch of its own. An agent starting a fork branch of the same
    // name is starting a new branch here: upstream is somebody else's repository,
    // and basing on its tip would drag the newer trunk into the next cut through
    // one member - the thing `start` exists not to do.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let shared_base = commit_at(&lab, "main@origin");
    lab.advance_upstream("upstream advance\n");
    lab.upstream_branch(
        "feature/dataframe",
        "theirs.txt",
        "somebody's upstream branch\n",
    );
    let (home, _consumer) = release_test_home(&lab);
    let theirs = commit_at(&lab, "feature/dataframe@upstream");

    let output = knives_start(&lab, &home, "feature/dataframe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "start failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feature-dataframe");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        shared_base.as_str(),
        "a name upstream happens to use must start on the shared base, not upstream's tip {}",
        theirs.short()
    );
    assert!(stdout.contains("(the release's shared base)"), "{stdout}");
}

#[test]
fn a_forced_start_on_a_divergent_branch_refuses_before_seizing_the_claim() {
    // Given: another owner's claim on a branch whose bookmark is divergent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    {
        let mut store = Store::open_for_update(home.path().join("state.json")).expect("open store");
        let held = knives::commands::claim::Identity {
            owner: "other-agent".to_owned(),
            kind: OwnerKind::OsUser,
        };
        let _ = store.claim(
            &knives::ids::BranchTarget::new(
                knives::ids::RepoName::new("demo"),
                BranchName::new("feat/alpha"),
            ),
            &held,
            "theirs",
        );
        store.save().expect("save claim");
    }
    lab.rewrite_in_both_clones("feat/alpha");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/alpha",
            "--repo",
            "demo",
            "--force",
            "--why",
            "seize",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run forced start");

    // Then: refused with the tips named, nothing seized, no workspace made -
    // a seized claim with no workspace would leave the branch held and the
    // agent one more `--force` from the work.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("feat/alpha is divergent (2 tips:"),
        "{stderr}"
    );
    let store = Store::open(home.path().join("state.json")).expect("reopen store");
    let claim = store
        .claims(Some(&knives::ids::RepoName::new("demo")))
        .into_iter()
        .find(|claim| claim.branch == "feat/alpha")
        .expect("the claim is still held");
    assert_eq!(
        claim.owner, "other-agent",
        "the claim was seized: {claim:?}"
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-alpha")
            .exists(),
        "no workspace may be created for a branch with no one tip"
    );
}

#[test]
fn start_on_a_divergent_branch_refuses_and_names_the_tips() {
    // A bookmark with two tips has no one commit to continue from; the agent
    // picks, then starts again.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    lab.rewrite_in_both_clones("feat/alpha");

    let output = knives_start(&lab, &home, "feat/alpha");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("feat/alpha is divergent (2 tips:")
            && stderr.contains("jj bookmark set feat/alpha"),
        "{stderr}"
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-alpha")
            .exists(),
        "no workspace may be created for a branch with no one tip"
    );
}

#[test]
fn start_makes_a_branch_pinned_only_by_an_untracked_remote_ref_rebasable() {
    // Given: our branch is pushed, and another clone built on top of it and
    // pushed too — the shape a superseded release cut or another fork's pull
    // request head takes. After the fetch that work is an untracked remote
    // bookmark here, and jj's default `immutable_heads()` freezes our own tip
    // beneath it, so the rebase a maintainer asked for is refused.
    let lab = Lab::new();
    lab.branch("feat/ours", "ours.txt", "ours\n");
    lab.push_branch("feat/ours");
    lab.foreign_origin_branch("feat/ours@origin", "theirs", "theirs\n");
    lab.advance_upstream("upstream moved on\n");
    lab.fetch_work();
    assert_eq!(
        lab.revision(&lab.work, "feat/ours", "immutable"),
        "true",
        "the fixture must reproduce jj's default pin or the test proves nothing"
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: any branch is started in the managed fork
    let output = knives_start(&lab, &home, "feat/gamma");

    // Then: the write is disclosed, our tip is mutable again, upstream's trunk
    // is not (jj's `trunk()` here is `main@origin`, pinned by the clone), and the
    // rebase runs
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "jj immutable_heads() written to demo's repository config: trunk() | tags() | remote_bookmarks(exact:\"main\", exact:\"upstream\") | remote_bookmarks(exact:\"main\", exact:\"origin\")"
        ),
        "the rule write must be disclosed: {stdout}"
    );
    assert_eq!(
        lab.revision(&lab.work, "feat/ours", "immutable"),
        "false",
        "an untracked remote ref must not freeze commits in a managed fork"
    );
    assert_eq!(
        lab.revision(&lab.work, "main@upstream", "immutable"),
        "true",
        "upstream's trunk stays immutable whatever `trunk()` resolves to"
    );
    lab.jj_work(["rebase", "-b", "feat/ours", "-d", "main@upstream"]);
    let parent = lab.revision(&lab.work, "feat/ours-", "commit_id");
    assert_eq!(parent, commit_at(&lab, "main@upstream").as_str());
    // And: a second start writes nothing, because the rule is now stated
    let again = knives_start(&lab, &home, "feat/delta");
    assert!(again.status.success(), "{again:?}");
    assert!(
        !String::from_utf8_lossy(&again.stdout).contains("immutable_heads()"),
        "a stated rule is written once: {}",
        String::from_utf8_lossy(&again.stdout)
    );
}

#[test]
fn start_leaves_a_repo_level_immutable_heads_rule_a_human_set() {
    // Given: a rule already stated in the repository's own jj config — somebody's
    // decision, which `status` reports when it differs and nothing overwrites.
    // jj's documented table form, so a rule is recognised by its key, not its shape.
    let lab = Lab::new();
    lab.jj_work([
        "config",
        "set",
        "--repo",
        "revset-aliases.\"immutable_heads()\"",
        "{ definition = \"trunk() | tags() | bookmarks(exact:\\\"keep\\\")\", doc = \"keep is pinned\" }",
    ]);
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started
    let output = knives_start(&lab, &home, "feat/gamma");

    // Then: the stated rule still governs jj here, and nothing claims to have been written
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("immutable_heads()"),
        "nothing was written, so nothing is disclosed: {stdout}"
    );
    lab.branch("keep", "keep.txt", "keep\n");
    assert_eq!(
        lab.revision(&lab.work, "keep", "immutable"),
        "true",
        "the human's rule, not the fork's, decides what is immutable"
    );
}

#[test]
fn start_refreshes_the_rule_it_wrote_when_the_entry_moves_on() {
    // Given: knives' own earlier write — recognisable by its `doc` — stating a
    // rule this entry no longer produces, as after a registry change
    let lab = Lab::new();
    lab.jj_work([
        "config",
        "set",
        "--repo",
        "revset-aliases.\"immutable_heads()\"",
        &format!(
            "{{ definition = \"trunk() | tags()\", doc = \"{}\" }}",
            knives::jj::KNIVES_IMMUTABLE_HEADS_DOC
        ),
    ]);
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started
    let output = knives_start(&lab, &home, "feat/gamma");

    // Then: the stale rule is replaced by the entry's, and the line says refreshed
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "jj immutable_heads() refreshed in demo's repository config: trunk() | tags() | remote_bookmarks(exact:\"main\", exact:\"upstream\") | remote_bookmarks(exact:\"main\", exact:\"origin\")"
        ),
        "was: {stdout}"
    );
    let stated = lab.jj_work_output([
        "config",
        "list",
        "--repo",
        "revset-aliases.\"immutable_heads()\"",
    ]);
    assert!(
        stated.contains("exact:\\\"origin\\\"") || stated.contains("exact:\"origin\""),
        "the entry's rule must now be stated: {stated}"
    );
}

#[test]
fn start_says_when_the_forks_rule_shadows_a_user_level_one() {
    // Given: a human's rule in jj's user layer and none stated for the repository.
    // The repo layer resolves above it, so writing the fork's rule shadows it here.
    let lab = Lab::new();
    let user_config = lab.work.parent().expect("parent").join("user-jj.toml");
    std::fs::write(
        &user_config,
        "[revset-aliases]\n\"immutable_heads()\" = \"none()\"\n",
    )
    .expect("write user config");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started with that user config in force
    let output = start_command(&lab, &home, "feat/gamma")
        .env("JJ_CONFIG", &user_config)
        .output()
        .expect("run knives start");

    // Then: the write happens and names the rule it shadows
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("(shadows the user-level rule none() here)"),
        "the shadowed rule must be named: {stdout}"
    );
}
