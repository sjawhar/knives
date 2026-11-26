//! `knives release cut` recording a fixed release branch's published commit:
//! which parent is a carrier, how each member's lines are read, and the
//! remedy past a published commit, each run to its end with the real binary.
//!
//! The record reads each member against the recorded write the published
//! commit follows, never against that commit itself. A parent is a carrier
//! only where it is an earlier write of the branch the ledger records; a
//! commit none of whose parents is one, while a parent holds one, is
//! refused, naming that parent, until the release it was made on is
//! recorded first with `cut --at`. Past a published commit the newest write
//! lacks, the remedy records that commit as it is, each drop named, then a
//! merge of it and a branch that carries what was dropped.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::CommitId;
use lab::{Lab, commit_at, knives_release, release_parents, release_test_home};

/// A home whose registry names `integration` the fixed release branch.
fn fixed_home(lab: &Lab) -> tempfile::TempDir {
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

fn both(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Push `integration` and fetch it back; its published commit.
fn publish(lab: &Lab) -> CommitId {
    lab.push_branch("integration");
    lab.fetch_work();
    let published = commit_at(lab, "integration@origin");
    assert_eq!(commit_at(lab, "integration"), published, "fixture: publish");
    published
}

/// `integration` moved to a merge of `parents` made by hand, `edit` run on
/// its working copy first; the working copy off it. Its commit.
fn compose(lab: &Lab, parents: &[&str], edit: impl FnOnce(&std::path::Path)) -> CommitId {
    let mut args = vec!["new"];
    for parent in parents {
        args.extend(["-r", *parent]);
    }
    args.extend(["-m", "integration, composed by hand"]);
    lab::jj_in(lab.work_path(), &args);
    edit(lab.work_path());
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

/// A bookmark `name` at one commit on `base` that adds `file`.
fn commit_on(lab: &Lab, name: &str, base: &str, file: &str) -> CommitId {
    lab.jj_work(["new", "-r", base, "-m", name]);
    std::fs::write(lab.work_path().join(file), format!("{name}\n")).expect("write file");
    lab.jj_work(["bookmark", "create", name, "-r", "@"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    commit_at(lab, name)
}

fn entries(home: &tempfile::TempDir) -> Vec<knives::ledger::Entry> {
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

fn newest_event(home: &tempfile::TempDir) -> knives::ledger::Entry {
    entries(home)
        .into_iter()
        .rev()
        .find(|entry| entry.subject.as_deref() == Some("integration"))
        .expect("an event of integration")
}

fn records_carrier(event: &knives::ledger::Entry, commit: &CommitId) -> bool {
    event.parents.iter().any(|parent| {
        parent.commit == commit.as_str()
            && parent.branches == ["integration earlier write (carrier, not a member)"]
    })
}

fn records_member(event: &knives::ledger::Entry, commit: &CommitId) -> bool {
    event
        .evidence
        .iter()
        .skip(1)
        .any(|member| member == commit.as_str())
        && !records_carrier(event, commit)
}

/// Whether `revision`'s tree holds `file`: `jj file show` exits 0 with a
/// warning for a path it does not find, so the test is the bytes it prints.
fn has_file(lab: &Lab, revision: &str, file: &str) -> bool {
    let output = std::process::Command::new("jj")
        .args(["--ignore-working-copy", "file", "show", "-r", revision])
        .arg(format!("root:{file}"))
        .current_dir(lab.work_path())
        .output()
        .expect("run jj file show");
    output.status.success() && !output.stdout.is_empty()
}

/// Each value the `  name each drop: --allow-drop …` line of `out` gives,
/// as `--allow-drop VALUE` arguments.
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

/// releases.md's fast-forward recipe for a fixed branch, as written: every
/// parent in order, then the published head; the tree put back; adopt with
/// its --why; push.
fn recipe_publish(lab: &Lab, home: &tempfile::TempDir) -> CommitId {
    lab.fetch_work();
    let old = commit_at(lab, "integration@origin");
    let pre = commit_at(lab, "integration");
    let mut args: Vec<String> = vec!["rebase".into(), "-r".into(), "integration".into()];
    for parent in release_parents(lab, "integration") {
        args.extend(["-d".into(), parent.as_str().to_owned()]);
    }
    args.extend(["-d".into(), old.as_str().to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    lab::jj_in(lab.work_path(), &args);
    lab.jj_work(["restore", "--from", pre.as_str(), "--into", "integration"]);
    let why = format!(
        "fast-forward of {} onto the published head {}",
        pre.as_str(),
        old.as_str()
    );
    let adopted = knives_release(lab, home, &["adopt", "integration", "--why", why.as_str()]);
    assert!(
        adopted.status.success(),
        "fixture: recipe adopt: {}",
        both(&adopted)
    );
    let published = publish(lab);
    assert_eq!(
        release_parents(lab, "integration").last(),
        Some(&old),
        "fixture: the recipe puts the published head last"
    );
    published
}

#[test]
fn a_landing_placed_first_by_the_fast_forward_recipe_stays_a_member_through_two_edits() {
    // Given: W0, a cut of `feat/alpha`, pushed; a release composed by hand
    // of W0, then `feat/land` (a commit on W0, as 26 of oh-my-pi Cut10's 27
    // members are built on a recorded write), then `feat/beta`, pushed and
    // recorded by the cut; `feat/gamma` included and published by
    // releases.md's fast-forward recipe, so `feat/land` is the published
    // commit's first parent; then `feat/delta` included (edit 1).
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w0 = publish(&lab);
    let land = commit_on(&lab, "feat/land", w0.as_str(), "land.txt");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    compose(&lab, &[w0.as_str(), "feat/land", "feat/beta"], |_| {});
    publish(&lab);
    let cut = knives_release(&lab, &home, &["cut"]);
    assert_ne!(cut.status.code(), Some(3), "fixture: {}", both(&cut));
    let event = newest_event(&home);
    assert!(
        records_carrier(&event, &w0) && records_member(&event, &land),
        "fixture: W0 a carrier, the landing a member: {event:?}"
    );
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "fixture: {}", both(&included));
    recipe_publish(&lab, &home);
    assert_eq!(
        release_parents(&lab, "integration").first(),
        Some(&land),
        "fixture: the landing is the published commit's first parent"
    );
    lab.branch("feat/delta", "delta.txt", "delta\n");
    let edit1 = knives_release(&lab, &home, &["include", "feat/delta"]);
    assert!(edit1.status.success(), "fixture: {}", both(&edit1));

    // When: the release's members are read, and `feat/eps` is included
    // (edit 2), with no push since the recipe's.
    let members = knives_release(&lab, &home, &["members", "integration"]);
    lab.branch("feat/eps", "eps.txt", "eps\n");
    let edit2 = knives_release(&lab, &home, &["include", "feat/eps"]);

    // Then: the landing is a member, never an earlier write of the release:
    // `members` lists it as a member, edit 2 keeps it as a parent, and the
    // release keeps its file.
    let listed = both(&members);
    let line = listed
        .lines()
        .find(|line| line.contains(land.short()))
        .unwrap_or_else(|| panic!("members names no {}: {listed}", land.short()));
    assert!(!line.contains("carrier"), "{listed}");
    assert!(edit2.status.success(), "{}", both(&edit2));
    assert!(
        release_parents(&lab, "integration").contains(&land),
        "edit 2 left the landing out of the release's parents: {}",
        both(&edit2)
    );
    assert!(has_file(&lab, "integration", "land.txt"));
    assert!(has_file(&lab, "integration", "eps.txt"));
    assert!(!has_file(&lab, "integration", "no-such-file.txt"));
}

#[test]
fn a_landing_on_an_older_write_placed_first_stays_a_member_and_its_drop_is_refused() {
    // Given: W1, a cut of `feat/alpha`, pushed; `feat/beta` included (W2),
    // pushed; `feat/delta`, a landing built on W1, not the newest write;
    // the release published by hand as a merge of `feat/delta` (placed
    // first), W2 and `feat/eps`, and cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both(&included));
    let w2 = publish(&lab);
    let delta = commit_on(&lab, "feat/delta", w1.as_str(), "delta.txt");
    lab.branch("feat/eps", "eps.txt", "eps\n");
    compose(&lab, &["feat/delta", w2.as_str(), "feat/eps"], |_| {});
    publish(&lab);
    let cut = knives_release(&lab, &home, &["cut"]);
    assert_ne!(cut.status.code(), Some(3), "{}", both(&cut));
    let event = newest_event(&home);

    // When: the release is published by hand again, as a merge of that one
    // and `feat/zeta` that leaves `delta.txt` out, and cut.
    lab.branch("feat/zeta", "zeta.txt", "zeta\n");
    compose(&lab, &["integration", "feat/zeta"], |work| {
        std::fs::remove_file(work.join("delta.txt")).expect("remove delta.txt");
    });
    publish(&lab);
    let before = entries(&home).len();
    let dropped = knives_release(&lab, &home, &["cut"]);

    // Then: `feat/delta` was recorded as a member, not as a carrier, so the
    // cut that leaves its file out is refused, names it, and writes nothing.
    assert!(
        records_member(&event, &delta) && !records_carrier(&event, &delta),
        "{event:?}"
    );
    let out = both(&dropped);
    assert_eq!(dropped.status.code(), Some(3), "{out}");
    assert!(
        out.contains(delta.short()) || out.contains("feat/delta"),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_landing_on_an_older_write_placed_first_with_no_bookmark_stays_a_member() {
    // Given: W1, a cut of `feat/alpha`, pushed; `feat/beta` included (W2),
    // pushed; a landing built on W1, its bookmark gone; the release
    // published by hand as a merge of that landing (placed first) and W2.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both(&included));
    let w2 = publish(&lab);
    let landing = commit_on(&lab, "feat/delta", w1.as_str(), "delta.txt");
    compose(&lab, &[landing.as_str(), w2.as_str()], |_| {});
    lab.jj_work(["bookmark", "delete", "feat/delta"]);
    publish(&lab);

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: the landing descends only from W1, not from W2, the newest
    // write, so it is a member, and W2 the carrier.
    assert_ne!(cut.status.code(), Some(3), "{}", both(&cut));
    let event = newest_event(&home);
    assert!(
        records_member(&event, &landing) && records_carrier(&event, &w2),
        "{event:?}"
    );
}

#[test]
fn a_landing_with_no_recorded_write_beside_it_is_refused_whatever_its_bookmark() {
    // Given: W1, a cut of `feat/alpha`, pushed; `feat/land`, a landing on
    // W1, pushed with its bookmark; the release published by hand as a merge
    // of `feat/land` and `feat/beta`, W1 no parent of it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    let land = commit_on(&lab, "feat/land", w1.as_str(), "land.txt");
    lab.push_branch("feat/land");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    compose(&lab, &["feat/land", "feat/beta"], |_| {});
    let published = publish(&lab);
    let before = entries(&home).len();

    // When: a plain cut runs; then the forge deletes the merged branch and a
    // fetch removes every ref of it, as GitHub's "delete head branch" does,
    // and the cut runs again.
    let bookmarked = knives_release(&lab, &home, &["cut"]);
    let origin = lab.temp_path().join("origin.git");
    let deleted = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&origin)
        .args(["update-ref", "-d", "refs/heads/feat/land"])
        .output()
        .expect("run git update-ref");
    assert!(deleted.status.success(), "fixture: {}", both(&deleted));
    lab.fetch_work();
    assert!(
        lab.jj_work_output(["bookmark", "list", "--all-remotes", "feat/land"])
            .trim()
            .is_empty(),
        "fixture: a ref of the landing is left"
    );
    let unbookmarked = knives_release(&lab, &home, &["cut"]);

    // Then: both are refused, name the landing as a parent that holds W1
    // and is no write, and write nothing: neither its bookmark nor its
    // place makes it a member or a release.
    for output in [&bookmarked, &unbookmarked] {
        let out = both(output);
        assert_eq!(output.status.code(), Some(3), "{out}");
        assert!(
            out.contains(&format!(
                "{}, a parent of {}, holds integration's earlier write {}",
                land.short(),
                published.short(),
                w1.short()
            )),
            "{out}"
        );
        assert!(
            out.contains("record it first (`knives release cut --at <commit>`)"),
            "{out}"
        );
    }
    assert_eq!(entries(&home).len(), before);
}

/// The first cut of `feat/alpha` (two lines), pushed; then `integration`
/// published by hand as a merge of that cut and `feat/gamma`, new, with
/// `edit` run on the merge. Returns the lab, its home, the published merge
/// and `feat/gamma`'s commit.
fn published_over_the_first_cut(
    edit: impl FnOnce(&std::path::Path),
) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha one\nalpha two\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma one\ngamma two\n");
    compose(&lab, &["integration", "feat/gamma"], edit);
    let published = publish(&lab);
    let gamma = commit_at(&lab, "feat/gamma");
    (lab, home, published, gamma)
}

#[test]
fn a_published_merge_that_drops_a_new_members_line_is_refused_until_allow_drop_names_it() {
    // Given: [`published_over_the_first_cut`], whose merge keeps `gamma
    // one` and drops `gamma two`.
    let (lab, home, published, gamma) = published_over_the_first_cut(|work| {
        std::fs::write(work.join("gamma.txt"), "gamma one\n").expect("write gamma.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let refused = knives_release(&lab, &home, &["cut"]);

    // Then: `feat/gamma` is read against the cut the published merge
    // follows, which never recorded it, so its lost line is missing: the
    // cut is refused, names the line and the member's flag, and writes
    // nothing.
    let out = both(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(out.contains("gamma two"), "{out}");
    assert!(
        out.contains(&format!(
            "  name each drop: --allow-drop {}\n",
            gamma.short()
        )),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");

    // When: the cut runs with that flag.
    let recorded = cut_dropping(&lab, &home, &named_drops(&out));

    // Then: the published merge is recorded, and its event names the member
    // whose lines it lacks.
    let out = both(&recorded);
    assert_ne!(recorded.status.code(), Some(3), "{out}");
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&published.as_str().to_owned()));
    assert!(
        event.text.contains(&format!(
            "; without the lines of 1 member(s) that it lacks: feat/gamma@{}",
            gamma.short()
        )),
        "{event:?}"
    );
}

#[test]
fn a_published_merge_that_drops_a_recorded_members_line_is_refused() {
    // Given (control): [`published_over_the_first_cut`], whose merge drops
    // `alpha two`, a line of the cut's own member.
    let (lab, home, _, _) = published_over_the_first_cut(|work| {
        std::fs::write(work.join("alpha.txt"), "alpha one\n").expect("write alpha.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let refused = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, and nothing is written.
    let out = both(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(out.contains("alpha two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_published_merge_that_keeps_its_new_member_whole_is_recorded() {
    // Given (control): [`published_over_the_first_cut`], whose merge keeps
    // every line.
    let (lab, home, published, gamma) = published_over_the_first_cut(|_| {});

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: it records the merge with `feat/gamma` a member, and names no
    // divergence and no drop.
    let out = both(&cut);
    assert_ne!(cut.status.code(), Some(3), "{out}");
    assert!(!out.contains("diverges"), "{out}");
    assert!(!out.contains("--allow-drop"), "{out}");
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&published.as_str().to_owned()));
    assert!(records_member(&event, &gamma), "{event:?}");
}

/// The third review's P12 shape: a cut of `feat/alpha` pushed; `feat/beta`
/// included (W2) and pushed; `feat/gamma` included (W3) and not pushed, the
/// bookmark moved back to W2; then `integration` published by hand as a
/// merge of W2 and `feat/delta`. Returns the lab, its home, the published
/// merge and `feat/gamma`'s commit.
fn published_beside_the_newest_write() -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    publish(&lab);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both(&included));
    let w2 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma one\ngamma two\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{}", both(&included));
    lab.jj_work([
        "bookmark",
        "set",
        "integration",
        "-r",
        w2.as_str(),
        "--allow-backwards",
    ]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    compose(&lab, &[w2.as_str(), "feat/delta"], |_| {});
    let published = publish(&lab);
    let gamma = commit_at(&lab, "feat/gamma");
    (lab, home, published, gamma)
}

#[test]
fn a_fix_forward_that_drops_a_line_of_the_newest_writes_member_is_refused() {
    // Given: [`published_beside_the_newest_write`]; the plain cut refused
    // and run again with the drops it names, which records the published
    // merge; then the fix-forward made with `feat/gamma` as the refusal
    // prints it, except that its merge drops `gamma two`, and pushed.
    let (lab, home, published, _) = published_beside_the_newest_write();
    let refused = knives_release(&lab, &home, &["cut"]);
    let out = both(&refused);
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "which records {} as it is and names each drop",
            published.short()
        )),
        "{out}"
    );
    let recorded = cut_dropping(&lab, &home, &named_drops(&out));
    assert_ne!(recorded.status.code(), Some(3), "{}", both(&recorded));
    let message = format!("integration, fixed forward on {}", published.short());
    lab.jj_work([
        "new",
        published.short(),
        "feat/gamma",
        "-m",
        message.as_str(),
    ]);
    std::fs::write(lab.work_path().join("gamma.txt"), "gamma one\n").expect("write gamma.txt");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "trunk()"]);
    publish(&lab);
    let before = entries(&home).len();

    // When: the cut runs again.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: `feat/gamma` is read against the published merge the
    // fix-forward follows, which never recorded it, so the line its merge
    // dropped is missing: the cut is refused, names it, and writes nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("gamma two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn the_remedy_on_a_fresh_published_composition_records_it_then_the_fix_forward_as_its_carrier() {
    // Given: W0, a cut of `feat/alpha`, pushed; `feat/beta` included, the
    // newest write, never pushed; then `integration` composed afresh by hand
    // of `feat/alpha` and `feat/gamma`, which holds no recorded write, as
    // oh-my-pi's Cut7 was, and pushed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w0 = publish(&lab);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(included.status.success(), "{}", both(&included));
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &["feat/alpha", "feat/gamma"], |_| {});
    let published = publish(&lab);
    let reaches_w0 = format!("{} & ::{}", w0.as_str(), published.as_str());
    assert!(
        lab.jj_work_output([
            "log",
            "--no-graph",
            "-r",
            reaches_w0.as_str(),
            "-T",
            "commit_id"
        ])
        .is_empty(),
        "fixture: the fresh composition holds no recorded write"
    );

    // When: the plain cut runs; then again with the drops it names.
    let refused = knives_release(&lab, &home, &["cut"]);
    let out = both(&refused);
    let recorded = cut_dropping(&lab, &home, &named_drops(&out));

    // Then: the refusal says to record the published composition first, and
    // the second run records it with its own parents as members.
    assert_eq!(refused.status.code(), Some(3), "{out}");
    assert!(!out.contains("--allow-backwards"), "{out}");
    assert!(
        out.contains(&format!(
            "which records {} as it is and names each drop",
            published.short()
        )),
        "{out}"
    );
    assert_ne!(recorded.status.code(), Some(3), "{}", both(&recorded));
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&published.as_str().to_owned()));
    assert!(
        records_member(&event, &commit_at(&lab, "feat/gamma")),
        "{event:?}"
    );

    // When: the fix-forward is made with `feat/beta` as the refusal prints
    // it, pushed, and cut.
    let message = format!("integration, fixed forward on {}", published.short());
    lab.jj_work([
        "new",
        published.short(),
        "feat/beta",
        "-m",
        message.as_str(),
    ]);
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new", "trunk()"]);
    let forward = publish(&lab);
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: it records the merge with the published composition, now the
    // newest recorded write, as its carrier and `feat/beta` as a member.
    let out = both(&cut);
    assert_ne!(cut.status.code(), Some(3), "{out}");
    assert!(!out.contains("--allow-drop"), "{out}");
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&forward.as_str().to_owned()));
    assert!(records_carrier(&event, &published), "{event:?}");
    assert!(
        records_member(&event, &commit_at(&lab, "feat/beta")),
        "{event:?}"
    );
}

/// A commit on `base` that writes `files`, its bookmark `name` created there
/// or moved there; the working copy off it. Its commit.
fn commit_files(lab: &Lab, name: &str, base: &str, files: &[(&str, &str)]) -> CommitId {
    lab.jj_work(["new", "-r", base, "-m", name]);
    for (file, content) in files {
        std::fs::write(lab.work_path().join(file), content).expect("write file");
    }
    lab.jj_work(["bookmark", "set", name, "-r", "@", "--allow-backwards"]);
    lab.jj_work(["new", "-r", "main@origin"]);
    commit_at(lab, name)
}

/// W0, a cut of `feat/alpha`, pushed; then P1, published by hand as a merge
/// of W0 and `feat/beta`, a release the ledger never records, as oh-my-pi's
/// Cut10 is. Returns the lab, its home and P1.
fn unrecorded_release_over_the_first_cut() -> (Lab, tempfile::TempDir, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w0 = publish(&lab);
    lab.branch("feat/beta", "beta.txt", "beta one\nbeta two\n");
    compose(&lab, &[w0.as_str(), "feat/beta"], |_| {});
    let p1 = publish(&lab);
    (lab, home, p1)
}

#[test]
fn an_unrecorded_release_under_the_published_commit_is_refused_until_it_is_recorded_first() {
    for release_last in [false, true] {
        // Given: [`unrecorded_release_over_the_first_cut`], a lane's base
        // branch at P1, and `feat/land`, a landing on W0 whose bookmark is
        // gone; then P2 published by hand as a merge of P1 and the landing,
        // P1 first (a compose script's order) or last (knives' fast-forward
        // recipe's), as oh-my-pi's Cut11 is over Cut10.
        let (lab, home, p1) = unrecorded_release_over_the_first_cut();
        lab.jj_work(["bookmark", "create", "lane/base", "-r", p1.as_str()]);
        let w0 = release_parents(&lab, p1.as_str())[0].clone();
        let land = commit_files(&lab, "feat/land", w0.as_str(), &[("land.txt", "land\n")]);
        let parents = if release_last {
            [land.as_str(), p1.as_str()]
        } else {
            [p1.as_str(), land.as_str()]
        };
        compose(&lab, &parents, |_| {});
        let p2 = publish(&lab);
        lab.jj_work(["bookmark", "delete", "feat/land"]);
        let before = entries(&home).len();

        // When: a plain cut runs; then P1 is recorded with `--at`; then the
        // plain cut runs again.
        let plain = knives_release(&lab, &home, &["cut"]);
        let after_refusal = entries(&home).len();
        let older = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);
        let older_event = newest_event(&home);
        let cut = knives_release(&lab, &home, &["cut"]);

        // Then: the plain cut is refused, names P1 and the landing, each a
        // parent that holds W0 and is no write, whatever P1's place and its
        // bookmark, offers no `--allow-drop` for P1, and writes nothing.
        // `--at` records P1 with W0 its carrier; the cut then records P2
        // with P1 its carrier and the landing a member, never P1 a member.
        let out = both(&plain);
        assert_eq!(plain.status.code(), Some(3), "{release_last}: {out}");
        for parent in [&p1, &land] {
            assert!(
                out.contains(&format!(
                    "{}, a parent of {}, holds integration's earlier write {}",
                    parent.short(),
                    p2.short(),
                    w0.short()
                )),
                "{release_last}: {out}"
            );
        }
        assert!(
            !out.contains(&format!("--allow-drop {}", p1.short())),
            "{release_last}: {out}"
        );
        assert_eq!(after_refusal, before, "{release_last}: {out}");
        assert_ne!(older.status.code(), Some(3), "{}", both(&older));
        assert_eq!(older_event.evidence.first(), Some(&p1.as_str().to_owned()));
        assert!(records_carrier(&older_event, &w0), "{older_event:?}");
        let out = both(&cut);
        assert_ne!(cut.status.code(), Some(3), "{release_last}: {out}");
        let event = newest_event(&home);
        assert_eq!(event.evidence.first(), Some(&p2.as_str().to_owned()));
        assert!(records_carrier(&event, &p1), "{event:?}");
        assert!(records_member(&event, &land), "{event:?}");
        assert!(!records_member(&event, &p1), "{event:?}");
    }
}

#[test]
fn a_merge_landing_with_no_ref_placed_first_is_refused_never_read_as_a_carrier() {
    // Given: W1, a cut of `feat/alpha`, pushed; a landing made as oh-my-pi's
    // are, a merge of W1 (first) and `feat/x`'s line, with no ref of its
    // own; the release published by hand as a merge of that landing (placed
    // first) and `feat/beta`. A parent count cannot tell it from a release
    // made as a merge on W1.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/x", "x.txt", "x one\nx two\n");
    lab::jj_in(
        lab.work_path(),
        &[
            "new",
            "-r",
            w1.as_str(),
            "-r",
            "feat/x",
            "-m",
            "x landing on W1",
        ],
    );
    let landing = lab.jj_work_output(["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let landing = CommitId::new(landing.trim());
    lab.jj_work(["new", "-r", "main@origin"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    compose(&lab, &[landing.as_str(), "feat/beta"], |_| {});
    let published = publish(&lab);
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, names the landing as a parent that holds W1 and
    // is no write, and writes nothing: the landing is never recorded as an
    // earlier write, which no later gate would check.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains(&format!(
            "{}, a parent of {}, holds integration's earlier write {}",
            landing.short(),
            published.short(),
            w1.short()
        )),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

/// [`unrecorded_release_over_the_first_cut`], P1 then recorded with
/// `--at`; `feat/land`, a landing on P1 that adds `land.txt`; then P2,
/// published by hand as a merge of P1 and `feat/land`, with `edit` run on
/// it. Returns the lab, its home and the landing.
fn landing_on_a_recorded_release(
    edit: impl FnOnce(&std::path::Path),
) -> (Lab, tempfile::TempDir, CommitId) {
    let (lab, home, p1) = unrecorded_release_over_the_first_cut();
    let older = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);
    assert_ne!(older.status.code(), Some(3), "fixture: {}", both(&older));
    let land = commit_files(
        &lab,
        "feat/land",
        p1.as_str(),
        &[("land.txt", "land one\nland two\n")],
    );
    compose(&lab, &[p1.as_str(), "feat/land"], edit);
    publish(&lab);
    (lab, home, land)
}

#[test]
fn a_landing_whose_own_line_the_published_commit_drops_is_refused() {
    // Given: [`landing_on_a_recorded_release`], whose merge P2 drops `land
    // two`, a line of the landing's own.
    let (lab, home, land) = landing_on_a_recorded_release(|work| {
        std::fs::write(work.join("land.txt"), "land one\n").expect("write land.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: it is refused, names that line and the landing's flag, and
    // writes nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("land two"), "{out}");
    assert!(
        out.contains(&format!("--allow-drop {}", land.short())),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_landing_on_the_carrier_never_answers_for_a_member_under_it() {
    // Given: [`landing_on_a_recorded_release`], whose merge P2 drops `beta
    // two`, a line of `feat/beta`, P1's member. `feat/land` descends from
    // `feat/beta` only through P1.
    let (lab, home, _) = landing_on_a_recorded_release(|work| {
        std::fs::write(work.join("beta.txt"), "beta one\n").expect("write beta.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: the lost line is still read: the cut is refused naming it, and
    // writes nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("beta two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_cut_over_an_unpushed_hand_composition_is_refused_where_a_new_members_line_is_lost() {
    // Given: W1, a cut of `feat/alpha`, pushed; then `integration` composed
    // afresh by hand of `feat/alpha` and `feat/gamma` (five lines), its third
    // line left out, and not pushed: the composition in hand, which no knives
    // write or adopt measured. It is not built on W1, so the push-first
    // refusal does not apply.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "g1\ng2\ng3\ng4\ng5\n");
    compose(&lab, &["feat/alpha", "feat/gamma"], |work| {
        std::fs::write(work.join("gamma.txt"), "g1\ng2\ng4\ng5\n").expect("write gamma.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: `feat/gamma` is read against W1, the newest recorded write,
    // which never recorded it, so the lost line is missing: the cut is
    // refused, names it, and writes nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("‹g3›"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_fixed_branch_composition_on_the_published_release_refuses_until_pushed() {
    // Given: W1, a cut of `feat/alpha`, pushed; then an unpushed
    // composition on W1 with a whole new `feat/gamma`. Its own parent set
    // does not contain the published release's carrier after a candidate
    // rebuild, so building another cut would lose W1's recorded members from
    // the event.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha one\nalpha two\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let composition = compose(&lab, &[w1.as_str(), "feat/gamma"], |_| {});
    let before = entries(&home).len();

    // When: a plain cut runs before the composition is pushed.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: it refuses and writes nothing, because fixed-branch recording
    // takes the exact published commit. The remedy names the push before the
    // next cut.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains("push integration, then `knives release cut`"),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
    assert_eq!(commit_at(&lab, "integration"), composition, "{out}");
}

#[test]
fn a_member_the_published_commit_holds_through_its_carrier_stays_recorded() {
    // Given: W1, a cut of `feat/alpha` (two lines), pushed; P1, published by
    // hand as a merge of W1 and `feat/gamma`, and recorded; then P2,
    // published by hand as a merge of P1 and `feat/zeta`, without `alpha
    // two`, a line of W1's member that P1 holds only through W1.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha one\nalpha two\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &[w1.as_str(), "feat/gamma"], |_| {});
    let p1 = publish(&lab);
    let recorded = knives_release(&lab, &home, &["cut"]);
    assert_ne!(
        recorded.status.code(),
        Some(3),
        "fixture: {}",
        both(&recorded)
    );
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&p1.as_str().to_owned()));
    lab.branch("feat/zeta", "zeta.txt", "zeta\n");
    compose(&lab, &[p1.as_str(), "feat/zeta"], |work| {
        std::fs::write(work.join("alpha.txt"), "alpha one\n").expect("write alpha.txt");
    });
    publish(&lab);
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: P1's record kept `feat/alpha` a recorded member through its
    // carrier, so P2's cut is refused, names `alpha two`, and writes
    // nothing.
    assert!(
        records_member(&event, &commit_at(&lab, "feat/alpha")),
        "{event:?}"
    );
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("alpha two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_member_kept_by_a_record_stays_recorded_past_an_edit_and_the_next_record() {
    // Given: W1, a cut of `feat/alpha` (two lines), pushed; P1, published by
    // hand as a merge of W1 and `feat/gamma`, and recorded, which keeps
    // `feat/alpha` a recorded member through W1; then `include feat/zeta`,
    // whose edit event lists only its parents, pushed and recorded.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha one\nalpha two\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &[w1.as_str(), "feat/gamma"], |_| {});
    publish(&lab);
    let recorded = knives_release(&lab, &home, &["cut"]);
    assert_ne!(
        recorded.status.code(),
        Some(3),
        "fixture: {}",
        both(&recorded)
    );
    lab.branch("feat/zeta", "zeta.txt", "zeta\n");
    let included = knives_release(&lab, &home, &["include", "feat/zeta"]);
    assert_ne!(
        included.status.code(),
        Some(3),
        "fixture: {}",
        both(&included)
    );
    let edit = publish(&lab);
    let after_edit = knives_release(&lab, &home, &["cut"]);
    let event = newest_event(&home);

    // When: P3, published by hand as a merge of that edit and `feat/eta`
    // without `alpha two`, is cut.
    lab.branch("feat/eta", "eta.txt", "eta\n");
    compose(&lab, &[edit.as_str(), "feat/eta"], |work| {
        std::fs::write(work.join("alpha.txt"), "alpha one\n").expect("write alpha.txt");
    });
    publish(&lab);
    let before = entries(&home).len();
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: the edit's record kept `feat/alpha`, which it carries but does
    // not merge, so P3's cut is refused, names `alpha two`, and writes
    // nothing.
    assert_ne!(after_edit.status.code(), Some(3), "{}", both(&after_edit));
    assert_eq!(event.evidence.first(), Some(&edit.as_str().to_owned()));
    assert!(
        records_member(&event, &commit_at(&lab, "feat/alpha")),
        "{event:?}"
    );
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("alpha two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_record_on_the_newest_write_is_not_held_to_a_line_that_write_already_lacked() {
    // Given: W1, a cut of `feat/alpha` (two lines), pushed; P1, published by
    // hand as a merge of W1 and `feat/gamma` without `alpha two`, recorded
    // with that drop named; then P2, published by hand as a merge of P1 and
    // `feat/zeta`, a branch on the trunk, that keeps P1's tree: the shape of
    // the next release on oh-my-pi's Cut13 with a member off the trunk.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha one\nalpha two\n");
    let home = fixed_home(&lab);
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(first.status.success(), "{}", both(&first));
    let w1 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let p1 = compose(&lab, &[w1.as_str(), "feat/gamma"], |work| {
        std::fs::write(work.join("alpha.txt"), "alpha one\n").expect("write alpha.txt");
    });
    publish(&lab);
    let refused = knives_release(&lab, &home, &["cut"]);
    let recorded = cut_dropping(&lab, &home, &named_drops(&both(&refused)));
    assert_ne!(
        recorded.status.code(),
        Some(3),
        "fixture: {}",
        both(&recorded)
    );
    assert_eq!(
        newest_event(&home).evidence.first(),
        Some(&p1.as_str().to_owned())
    );
    lab.branch("feat/zeta", "zeta.txt", "zeta\n");
    let p2 = compose(&lab, &[p1.as_str(), "feat/zeta"], |_| {});
    publish(&lab);

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: `alpha two`, which P1 already lacked, is carried forward from
    // P1: P2 is recorded with P1 its carrier and no drop named.
    let out = both(&cut);
    assert_ne!(cut.status.code(), Some(3), "{out}");
    assert!(!out.contains("--allow-drop"), "{out}");
    let event = newest_event(&home);
    assert_eq!(event.evidence.first(), Some(&p2.as_str().to_owned()));
    assert!(records_carrier(&event, &p1), "{event:?}");
}

#[test]
fn a_change_by_hand_no_member_explains_refuses_the_record_of_a_published_commit() {
    // Given: [`published_over_the_first_cut`], whose merge also rewrites
    // the trunk's `README.md` and adds `stray.txt`, which no member adds.
    let (lab, home, _, _) = published_over_the_first_cut(|work| {
        std::fs::write(work.join("README.md"), "changed by hand\n").expect("write README.md");
        std::fs::write(work.join("stray.txt"), "stray\n").expect("write stray.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: the drift is measured from the cut the commit was made on, so
    // both files are named and nothing is written.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(
        out.contains("README.md") && out.contains("stray.txt"),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

/// [`unrecorded_release_over_the_first_cut`] (P1 = W0 + `feat/beta`), P1
/// then recorded with `--at`; then P2 published by hand as a merge of P1 and
/// `feat/gamma`, `edit` run on it. Returns the lab, its home, P2 and
/// `feat/beta`.
fn over_a_release_recorded_with_at(
    edit: impl FnOnce(&std::path::Path),
) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let (lab, home, p1) = unrecorded_release_over_the_first_cut();
    let older = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);
    assert_ne!(older.status.code(), Some(3), "fixture: {}", both(&older));
    let beta = commit_at(&lab, "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &[p1.as_str(), "feat/gamma"], edit);
    let p2 = publish(&lab);
    (lab, home, p2, beta)
}

#[test]
fn a_member_of_a_release_recorded_with_at_is_kept_by_the_next_record() {
    // Given: [`over_a_release_recorded_with_at`], P2 keeping every line.
    let (lab, home, p2, beta) = over_a_release_recorded_with_at(|_| {});

    // When: the cut runs; then P3 is published by hand as a merge of P2 and
    // `feat/zeta` that drops `beta two`, and cut.
    let recorded = knives_release(&lab, &home, &["cut"]);
    let event = newest_event(&home);
    lab.branch("feat/zeta", "zeta.txt", "zeta\n");
    compose(&lab, &[p2.as_str(), "feat/zeta"], |work| {
        std::fs::write(work.join("beta.txt"), "beta one\n").expect("write beta.txt");
    });
    publish(&lab);
    let before = entries(&home).len();
    let dropped = knives_release(&lab, &home, &["cut"]);

    // Then: P2's record keeps `feat/beta`, P1's member, through its carrier,
    // so P3's cut is refused naming `beta two`, and writes nothing.
    assert_ne!(recorded.status.code(), Some(3), "{}", both(&recorded));
    assert!(records_member(&event, &beta), "{event:?}");
    let out = both(&dropped);
    assert_eq!(dropped.status.code(), Some(3), "{out}");
    assert!(out.contains("beta two"), "{out}");
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_line_of_a_member_of_the_release_it_follows_the_record_lacks_is_refused() {
    // Given: [`over_a_release_recorded_with_at`], P2 dropping `beta two`, a
    // line of `feat/beta`, P1's member, that no member of P2 rewrites.
    let (lab, home, _, beta) = over_a_release_recorded_with_at(|work| {
        std::fs::write(work.join("beta.txt"), "beta one\n").expect("write beta.txt");
    });
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: refused, naming the line and the member's flag, and nothing is
    // written.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("beta two"), "{out}");
    assert!(
        out.contains(&format!("--allow-drop {}", beta.short())),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn a_line_a_later_member_kept_of_a_member_it_rewrites_is_still_audited() {
    // Given: [`unrecorded_release_over_the_first_cut`], P1 recorded with
    // `--at`; `feat/rewrite` (a commit on `feat/beta`, P1's member, that
    // rewrites `beta one`); and P2 published by hand as a merge of P1 and
    // `feat/rewrite` that also drops `beta two`, which `feat/rewrite` kept.
    let (lab, home, p1) = unrecorded_release_over_the_first_cut();
    let older = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);
    assert_ne!(older.status.code(), Some(3), "fixture: {}", both(&older));
    let rewrite = commit_files(
        &lab,
        "feat/rewrite",
        "feat/beta",
        &[("beta.txt", "beta ONE\nbeta two\n")],
    );
    compose(&lab, &[p1.as_str(), "feat/rewrite"], |work| {
        std::fs::write(work.join("beta.txt"), "beta ONE\n").expect("write beta.txt");
    });
    publish(&lab);
    let before = entries(&home).len();

    // When: a plain cut runs.
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: `feat/rewrite` answers for `feat/beta`'s lines it kept: the cut
    // is refused naming `beta two` and its flag, and writes nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("beta two"), "{out}");
    assert!(
        out.contains(&format!("--allow-drop {}", rewrite.short())),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

/// Two releases published one on the other and never recorded:
/// [`unrecorded_release_over_the_first_cut`] (P0 = W0 + `feat/beta`), a
/// lane's base branch at P0, and `feat/land`, a landing on W0 whose bookmark
/// is gone; P1 published as a merge of P0 and the landing; then P2 published
/// as a merge of P1 and `feat/gamma`, `edit` run on it. Returns the lab, its
/// home, P0, P1, P2, the landing and `feat/beta`.
fn two_unrecorded_releases(
    edit: impl FnOnce(&std::path::Path),
) -> (Lab, tempfile::TempDir, [CommitId; 3], CommitId, CommitId) {
    let (lab, home, p0) = unrecorded_release_over_the_first_cut();
    lab.jj_work(["bookmark", "create", "lane/base", "-r", p0.as_str()]);
    let w0 = release_parents(&lab, p0.as_str())[0].clone();
    let land = commit_files(&lab, "feat/land", w0.as_str(), &[("land.txt", "land\n")]);
    compose(&lab, &[p0.as_str(), "feat/land"], |_| {});
    let p1 = publish(&lab);
    lab.jj_work(["bookmark", "delete", "feat/land"]);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &[p1.as_str(), "feat/gamma"], edit);
    let p2 = publish(&lab);
    let beta = commit_at(&lab, "feat/beta");
    (lab, home, [p0, p1, p2], land, beta)
}

/// Each run of the walk shape of [`two_unrecorded_releases`] that must be
/// refused before P0 and P1 are recorded in order: a plain cut, which names
/// P1, then `--at P1`, which names P0 and the landing, each writing nothing.
fn refused_until_recorded_in_order(
    (lab, home): (&Lab, &tempfile::TempDir),
    [p0, p1, p2]: &[CommitId; 3],
    land: &CommitId,
) {
    let before = entries(home).len();
    let plain = knives_release(lab, home, &["cut"]);
    let newer_first = knives_release(lab, home, &["cut", "--at", p1.as_str()]);
    for (output, published, unknown) in [(&plain, p2, vec![p1]), (&newer_first, p1, vec![p0, land])]
    {
        let out = both(output);
        assert_eq!(output.status.code(), Some(3), "{out}");
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
    }
    assert_eq!(entries(home).len(), before);
    for older in [p0, p1] {
        let recorded = knives_release(lab, home, &["cut", "--at", older.as_str()]);
        assert_ne!(recorded.status.code(), Some(3), "{}", both(&recorded));
        assert_eq!(
            newest_event(home).evidence.first(),
            Some(&older.as_str().to_owned())
        );
    }
}

#[test]
fn releases_recorded_in_order_refuse_a_line_lost_below_them() {
    // Given: [`two_unrecorded_releases`], P2 dropping `beta two`, a line of
    // P0's member, as round 7's walk recorded it with only a note.
    let (lab, home, releases, land, beta) = two_unrecorded_releases(|work| {
        std::fs::write(work.join("beta.txt"), "beta one\n").expect("write beta.txt");
    });

    // When: P2 is cut, refused until P0 and P1 are recorded in order; then
    // cut again.
    refused_until_recorded_in_order((&lab, &home), &releases, &land);
    let before = entries(&home).len();
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: the record of P1 kept `feat/beta` through its carrier, so P2's
    // cut is refused, names `beta two` and `feat/beta`'s flag, and writes
    // nothing.
    let out = both(&cut);
    assert_eq!(cut.status.code(), Some(3), "{out}");
    assert!(out.contains("beta two"), "{out}");
    assert!(
        out.contains(&format!("--allow-drop {}", beta.short())),
        "{out}"
    );
    assert_eq!(entries(&home).len(), before, "{out}");
}

#[test]
fn releases_recorded_in_order_then_record_the_newest_that_keeps_every_line() {
    // Given (control): [`two_unrecorded_releases`], P2 keeping every line.
    let (lab, home, releases, land, beta) = two_unrecorded_releases(|_| {});

    // When: P2 is cut, refused until P0 and P1 are recorded in order; then
    // cut again.
    refused_until_recorded_in_order((&lab, &home), &releases, &land);
    let cut = knives_release(&lab, &home, &["cut"]);

    // Then: P2 is recorded with P1 its carrier, and its event keeps
    // `feat/beta` and the landing, the members of the releases under it.
    let out = both(&cut);
    assert_ne!(cut.status.code(), Some(3), "{out}");
    let event = newest_event(&home);
    assert_eq!(
        event.evidence.first(),
        Some(&releases[2].as_str().to_owned())
    );
    assert!(records_carrier(&event, &releases[1]), "{event:?}");
    assert!(records_member(&event, &beta), "{event:?}");
    assert!(records_member(&event, &land), "{event:?}");
}

#[test]
fn a_landing_under_a_published_release_is_never_taken_for_the_release_it_was_made_on() {
    // Given: [`unrecorded_release_over_the_first_cut`] (P0), recorded with
    // `--at`; `feat/land`, a commit on P0 that adds `land one` and `land
    // two`, its bookmark gone; P1 published by hand as a merge of the landing
    // (first) and `feat/delta`; then P2 as a merge of P1 and `feat/gamma`
    // that drops `land two`. P0 is no parent of P1.
    let (lab, home, p0) = unrecorded_release_over_the_first_cut();
    let older = knives_release(&lab, &home, &["cut", "--at", p0.as_str()]);
    assert_ne!(older.status.code(), Some(3), "fixture: {}", both(&older));
    let land = commit_files(
        &lab,
        "feat/land",
        p0.as_str(),
        &[("land.txt", "land one\nland two\n")],
    );
    lab.jj_work(["bookmark", "delete", "feat/land"]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    compose(&lab, &[land.as_str(), "feat/delta"], |_| {});
    let p1 = publish(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    compose(&lab, &[p1.as_str(), "feat/gamma"], |work| {
        std::fs::write(work.join("land.txt"), "land one\n").expect("write land.txt");
    });
    let p2 = publish(&lab);
    let before = entries(&home).len();

    // When: a plain cut runs; then `--at P1`.
    let plain = knives_release(&lab, &home, &["cut"]);
    let at_p1 = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);

    // Then: each is refused and writes nothing: the plain cut names P1, and
    // `--at P1` names the landing, each a parent that holds P0 and is no
    // write. The landing is never read as P1's release, so its lines are
    // never left unread.
    for (output, published, unknown) in [(&plain, &p2, &p1), (&at_p1, &p1, &land)] {
        let out = both(output);
        assert_eq!(output.status.code(), Some(3), "{out}");
        assert!(
            out.contains(&format!(
                "{}, a parent of {}, holds integration's earlier write {}",
                unknown.short(),
                published.short(),
                p0.short()
            )),
            "{out}"
        );
    }
    assert_eq!(entries(&home).len(), before);
}

#[test]
fn at_records_only_a_commit_the_publish_remote_holds_newer_than_the_newest_write() {
    // Given: [`unrecorded_release_over_the_first_cut`] (P1 = W0 +
    // `feat/beta`), P1 recorded by a plain cut; then `feat/gamma`, which the
    // publish remote does not hold.
    let (lab, home, p1) = unrecorded_release_over_the_first_cut();
    let recorded = knives_release(&lab, &home, &["cut"]);
    assert_ne!(
        recorded.status.code(),
        Some(3),
        "fixture: {}",
        both(&recorded)
    );
    let w0 = release_parents(&lab, p1.as_str())[0].clone();
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let gamma = commit_at(&lab, "feat/gamma");
    let before = entries(&home).len();

    // When: `--at` names `feat/gamma`, W0 and P1.
    let unheld = knives_release(&lab, &home, &["cut", "--at", gamma.as_str()]);
    let older = knives_release(&lab, &home, &["cut", "--at", w0.as_str()]);
    let same = knives_release(&lab, &home, &["cut", "--at", p1.as_str()]);

    // Then: each is refused and writes nothing: the publish remote does not
    // hold `feat/gamma`, W0 is in the history of the newest recorded write,
    // and P1 is that write.
    let expected = [
        (
            &unheld,
            format!(
                "integration@origin ({}) does not hold {}",
                p1.short(),
                gamma.short()
            ),
        ),
        (
            &older,
            format!(
                "it is integration's newest recorded write ({}) or in its history",
                p1.short()
            ),
        ),
        (&same, "nothing to cut".to_owned()),
    ];
    for (output, says) in expected {
        let out = both(output);
        assert_eq!(output.status.code(), Some(3), "{out}");
        assert!(out.contains(&says), "{says}: {out}");
    }
    assert_eq!(entries(&home).len(), before);
}
