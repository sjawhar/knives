//! A branch built on a release of a series: its history past the trunk
//! carries that release's merge, which the `stacked-history` check reads.
//!
//! The next release's `include` refuses a branch built on the series'
//! previous release: it must be rebased off the trunk. Preflight and status
//! name the release in such a branch's history, by each ref that names it,
//! local and on origin, and by its commit, for a release of the default
//! series and for one of series `old` alike, and so do not send the branch
//! to `knives sync` for upstream merges.

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

use lab::{Lab, commit_at, knives_release, release_test_home};

/// The series' first cut: the previous release of the release in hand.
const PREVIOUS: &str = "release/old/2026-08-04";

/// The series' next release, made by hand on [`PREVIOUS`], or cut.
const NEXT: &str = "release/old/2026-08-05";

/// Whether `revision` resolves in the lab's work checkout.
fn resolves(lab: &Lab, revision: &str) -> bool {
    knives::jj::Repo::open(lab.work_path())
        .expect("open work checkout")
        .resolve_commit(revision)
        .is_ok()
}

/// A lab whose upstream carries a second base, `old`, split from `main`: the
/// seed is their common ancestor, and each has moved on since.
fn two_base_lab() -> Lab {
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    lab
}

/// A branch forked from `base`, with its bookmark on its one commit.
fn branch_on(lab: &Lab, base: &str, name: &str, file: &str) {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write branch");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The release-test home with the registry entry given a series `old` on the
/// upstream branch `old`, plus whatever `extra` the test adds to that table.
fn series_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let (home, _consumer) = release_test_home(lab);
    let registry = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read registry");
    text.push_str("\n[repos.demo.series.old]\nbase = \"old\"\n");
    text.push_str(extra);
    std::fs::write(&registry, text).expect("write registry");
    home
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Series `old`: its first cut [`PREVIOUS`] of `fix/a` and `fix/b`, pushed
/// to origin and marked published by a `record: published` note, and three
/// branches on that cut ([`branches_on_previous`]).
fn previous_release_lab() -> (Lab, tempfile::TempDir) {
    let lab = two_base_lab();
    branch_on(&lab, "old@upstream", "fix/a", "a.txt");
    branch_on(&lab, "old@upstream", "fix/b", "b.txt");
    let home = series_home(&lab, "");
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", PREVIOUS]);
    assert!(cut.status.success(), "{}", text(&cut));
    lab.push_branch(PREVIOUS);
    lab.fetch_work();
    mark_published(&lab, &home, PREVIOUS);
    branches_on_previous(&lab);
    (lab, home)
}

/// Three branches on [`PREVIOUS`]: `del` deletes `a.txt`, which that
/// release carries, and `m6` and `m5` each add a file.
fn branches_on_previous(lab: &Lab) {
    lab.jj_work(["new", "-r", PREVIOUS, "-m", "del"]);
    std::fs::remove_file(lab.work_path().join("a.txt")).expect("delete a file the cut carries");
    lab.jj_work(["bookmark", "create", "del", "-r", "@"]);
    lab.jj_work(["new"]);
    branch_on(lab, PREVIOUS, "m6", "m6.txt");
    branch_on(lab, PREVIOUS, "m5", "m5.txt");
}

/// A `record: published` note on release `name`, naming its commit, as the
/// release owner writes one.
fn mark_published(lab: &Lab, home: &tempfile::TempDir, name: &str) {
    let commit = commit_at(lab, name);
    let noted = lab::knives(
        lab,
        home,
        &[
            "notch",
            name,
            "-m",
            "record: published as v1.0.0",
            "--evidence",
            commit.as_str(),
            "--repo",
            "demo",
        ],
    );
    assert!(noted.status.success(), "{}", text(&noted));
}

#[test]
fn without_stacked_an_include_stacked_on_the_previous_release_is_refused() {
    // Given: the same series and branches without the stacked key, and the
    // next release cut: it starts identical to the published first cut,
    // which is now its previous release.
    let (lab, home) = previous_release_lab();
    let cut = knives_release(&lab, &home, &["--series", "old", "cut", NEXT]);
    assert!(resolves(&lab, NEXT), "{}", text(&cut));

    // When: a branch built on the previous release is included.
    let include = knives_release(&lab, &home, &["--series", "old", "include", "del"]);

    // Then: the unstacked rule refuses it: its history past the trunk
    // carries the previous release's merge.
    let out = text(&include);
    assert_eq!(include.status.code(), Some(3), "{out}");
    assert!(out.contains("rebase it off the trunk"), "{out}");
}

/// The line of `out` that says branch `branch`'s history past the trunk
/// carries merges.
fn history_line<'a>(out: &'a str, branch: &str) -> &'a str {
    out.lines()
        .find(|line| line.contains(&format!("branch {branch}'s history past the trunk carries")))
        .unwrap_or_else(|| panic!("no history line for {branch}: {out}"))
}

/// The whole `!!` line preflight prints for a branch `branch` whose history
/// past the trunk carries one merge, the release `named` names.
fn one_release_line(branch: &str, named: &str) -> String {
    format!(
        "  !! branch {branch}'s history past the trunk carries 1 merge commit(s), so it \
         carries everything those merges carried; releases in that history: {named}"
    )
}

#[test]
fn preflight_names_an_unstacked_series_release_and_keeps_the_default_release_line() {
    // Given: a release of the default series, a merge of `feat/alpha` and
    // `feat/beta`, and `m5` built on it.
    let flat = Lab::new();
    flat.branch("feat/alpha", "alpha.txt", "alpha\n");
    flat.branch("feat/beta", "beta.txt", "beta\n");
    flat.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    branch_on(&flat, "release/2026-08-04", "m5", "m5.txt");
    let (flat_home, _consumer) = release_test_home(&flat);

    // When: preflight reads that fork.
    let out = text(&lab::knives(
        &flat,
        &flat_home,
        &["--text", "preflight", "demo"],
    ));

    // Then: its line for `m5` names that release and its commit, and nothing
    // more.
    let release = commit_at(&flat, "release/2026-08-04");
    assert_eq!(
        history_line(&out, "m5"),
        one_release_line("m5", &format!("release/2026-08-04 ({})", release.short())),
        "{out}"
    );

    // Given: a series `old` without `stacked`, whose first cut, pushed to
    // origin, has `m5` built on it.
    let (lab, home) = previous_release_lab();

    // When: preflight reads that fork.
    let out = text(&lab::knives(&lab, &home, &["--text", "preflight", "demo"]));

    // Then: its line for `m5` names that cut, local and on origin, and its
    // commit, as for the default series, not upstream merges for `knives
    // sync`.
    let previous = commit_at(&lab, PREVIOUS);
    assert_eq!(
        history_line(&out, "m5"),
        one_release_line(
            "m5",
            &format!("{PREVIOUS}, {PREVIOUS}@origin ({})", previous.short())
        ),
        "{out}"
    );
}

#[test]
fn status_names_an_unstacked_series_release_a_member_with_an_open_pull_request_is_built_on() {
    // Given: a series `old` without `stacked`, whose first cut, pushed to
    // origin, has `m5` built on it, and an open pull request from `m5`.
    let (lab, home) = previous_release_lab();
    let previous = commit_at(&lab, PREVIOUS);
    let shim = tempfile::tempdir().expect("create forge shim directory");
    let pulls = format!("[{}]", forge_shim::pull_record(7, "OPEN", "m5", None));
    forge_shim::install_snapshot_gh(shim.path(), &pulls, &[]);

    // When: status reads that fork, one line per finding.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-landed", "--verbose"])
        .current_dir(lab.work_path())
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", forge_shim::path_with_gh_shim(shim.path()))
        .output()
        .expect("run status with a forge shim");

    // Then: its stacked-history line for `m5` names that cut, local and on
    // origin, and its commit, as preflight does, and does not send m5 to
    // `knives sync`.
    let out = text(&output);
    let line = history_line(&out, "m5");
    assert!(
        line.ends_with(&format!(
            "; releases in that history: {PREVIOUS}, {PREVIOUS}@origin ({})",
            previous.short()
        )),
        "{out}"
    );
}
