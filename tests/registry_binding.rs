#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]
// allow: SIZE_OK: the binding contract in one place — roots, remotes, here, scan, resolve, and the verbs through the binary.

//! A checkout is bound to its registry entry by its `upstream` remote, from the
//! directory you stand in or by scanning `$HOME`.

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeMap;
use std::path::Path;

use knives::bind::{self, BindError, Unbound, Unresolved};
use knives::config::{Registry, RepoEntry};
use knives::ids::RepoName;
use lab::{git_repository, jj_checkout};

fn entry(upstream: &str, origin: &str) -> RepoEntry {
    RepoEntry {
        upstream: upstream.to_owned(),
        origin: origin.to_owned(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: vec![],
        workspaces: None,
    }
}

fn registry(entries: &[(&str, RepoEntry)]) -> Registry {
    Registry {
        repos: entries
            .iter()
            .map(|(name, entry)| ((*name).to_owned(), entry.clone()))
            .collect(),
        ..Registry::default()
    }
}

/// The fork the lab's work checkout is inside, bound against `registry` as a
/// verb's `Ground` binds it once.
fn here_at<'a>(registry: &'a Registry, cwd: &Path) -> Option<bind::Fork<'a>> {
    bind::here(registry, cwd).expect("read").ok()
}

#[test]
fn a_checkout_root_is_found_from_a_subdirectory_and_from_a_workspace() {
    let lab = lab::Lab::new();
    let nested = lab.work.join("src").join("deep");
    std::fs::create_dir_all(&nested).expect("nested directory");
    let workspace = lab.temp_path().join("ws");
    lab::jj(
        &lab.work,
        [
            "workspace",
            "add",
            "--name",
            "ws",
            workspace.to_str().expect("utf-8"),
        ],
    );

    let expected = lab.work.canonicalize().expect("canonical work");
    assert_eq!(bind::checkout_root(&nested), Some(expected.clone()));
    assert_eq!(bind::checkout_root(&workspace), Some(expected.clone()));
    // The workspace is its own nearest root; the checkout is the subdirectory's.
    let workspace_root = workspace.canonicalize().expect("canonical ws");
    assert_eq!(bind::nearest_root(&workspace), Some(workspace_root.clone()));
    assert_eq!(bind::checkout_of_root(&workspace_root), expected);
    assert_eq!(bind::checkout_of_root(&expected), expected);
    assert_eq!(
        bind::nearest_root(&nested),
        Some(lab.work.canonicalize().expect("canonical"))
    );
    assert_eq!(bind::checkout_root(lab.temp_path()), None);
}

#[test]
fn remotes_are_read_from_jj_checkouts_and_from_git_only_clones() {
    let lab = lab::Lab::new();
    let jj_remotes = bind::remotes(&lab.work).expect("jj remotes");
    assert_eq!(
        jj_remotes.get("upstream").map(String::as_str),
        Some(lab.upstream.to_str().expect("utf-8"))
    );
    assert!(jj_remotes.contains_key("origin"));

    let clone = lab.temp_path().join("plain-clone");
    git_repository(
        &clone,
        &[("origin", "https://forge.invalid/someone/tool.git")],
    );
    let clone_remotes = bind::remotes(&clone).expect("git remotes");
    assert_eq!(
        clone_remotes,
        BTreeMap::from([(
            "origin".to_owned(),
            "https://forge.invalid/someone/tool.git".to_owned()
        )])
    );

    let plain = lab.temp_path().join("not-a-repo");
    std::fs::create_dir_all(&plain).expect("plain dir");
    assert!(bind::remotes(&plain).is_err());
}

#[test]
fn here_binds_the_checkout_and_its_workspaces_to_their_entry() {
    let lab = lab::Lab::new();
    let registry = registry(&[(
        "demo",
        entry(
            lab.upstream.to_str().expect("utf-8"),
            "https://forge.invalid/acme/work.git",
        ),
    )]);
    let workspace = lab.temp_path().join("ws");
    lab::jj(
        &lab.work,
        [
            "workspace",
            "add",
            "--name",
            "ws",
            workspace.to_str().expect("utf-8"),
        ],
    );

    let from_checkout = bind::here(&registry, &lab.work)
        .expect("read")
        .expect("bound");
    assert_eq!(from_checkout.name, RepoName::new("demo"));
    assert_eq!(
        from_checkout.checkout.path,
        lab.work.canonicalize().expect("canonical")
    );
    assert!(from_checkout.checkout.is_jj());

    let from_workspace = bind::here(&registry, &workspace)
        .expect("read")
        .expect("bound");
    assert_eq!(from_workspace.checkout.path, from_checkout.checkout.path);
}

#[test]
fn the_nearest_repository_wins_when_one_is_nested_inside_another() {
    let lab = lab::Lab::new();
    let inner_git = lab.work.join("vendor").join("dep");
    git_repository(&inner_git, &[("upstream", "https://forge.invalid/org/dep")]);
    std::fs::create_dir_all(inner_git.join("src")).expect("nested source directory");
    let inner_jj = inner_git.join("nested").join("tool");
    jj_checkout(&inner_jj, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[
        (
            "demo",
            entry(
                lab.upstream.to_str().expect("utf-8"),
                "https://forge.invalid/acme/work.git",
            ),
        ),
        (
            "dep",
            entry(
                "https://forge.invalid/org/dep",
                "https://forge.invalid/acme/dep",
            ),
        ),
        (
            "tool",
            entry(
                "https://forge.invalid/org/tool",
                "https://forge.invalid/acme/tool",
            ),
        ),
    ]);

    let from_git = bind::here(&registry, &inner_git.join("src"))
        .expect("read")
        .expect("bound");
    assert_eq!(from_git.name, RepoName::new("dep"));
    assert_eq!(
        from_git.checkout.path,
        inner_git.canonicalize().expect("canonical")
    );
    assert!(!from_git.checkout.is_jj());
    let from_jj = bind::here(&registry, &inner_jj)
        .expect("read")
        .expect("bound");
    assert_eq!(from_jj.name, RepoName::new("tool"));
    let from_outer = bind::here(&registry, &lab.work.join("vendor"))
        .expect("read")
        .expect("bound");
    assert_eq!(from_outer.name, RepoName::new("demo"));
}

#[test]
fn here_refuses_outside_a_repository_without_upstream_and_when_unregistered() {
    let lab = lab::Lab::new();
    let registry = registry(&[(
        "demo",
        entry(
            "https://forge.invalid/org/elsewhere",
            "https://forge.invalid/acme/elsewhere",
        ),
    )]);

    let nowhere = lab.temp_path().join("nowhere");
    std::fs::create_dir_all(&nowhere).expect("plain dir");
    assert_eq!(
        bind::here(&registry, &nowhere).expect("read"),
        Err(Unbound::NotInsideARepository)
    );

    let no_upstream = lab.temp_path().join("no-upstream");
    git_repository(
        &no_upstream,
        &[("origin", "https://forge.invalid/me/thing")],
    );
    let unbound = bind::here(&registry, &no_upstream)
        .expect("read")
        .expect_err("unbound");
    assert!(matches!(unbound, Unbound::NoUpstream { .. }), "{unbound:?}");

    let unbound = bind::here(&registry, &lab.work)
        .expect("read")
        .expect_err("unbound");
    assert!(
        matches!(&unbound, Unbound::Unregistered { upstream, .. } if upstream == lab.upstream.to_str().expect("utf-8")),
        "{unbound:?}"
    );
}

#[test]
fn scan_finds_each_entry_once_skips_workspaces_dot_directories_symlinks_and_depth_four() {
    // Given: a home with a checkout at depth 1, one at depth 3, a workspace, a
    // dot-directory hiding a checkout, a checkout at depth 4, a git-only clone
    // whose upstream matches an entry, and a symlink to a directory outside
    // home that holds a matching checkout.
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let shallow = home.join("tool");
    jj_checkout(&shallow, &[("upstream", "https://forge.invalid/org/tool")]);
    let git_only = home.join("plain");
    git_repository(
        &git_only,
        &[("upstream", "https://forge.invalid/org/plain")],
    );
    let deep_parent = home.join("forks").join("work");
    std::fs::create_dir_all(&deep_parent).expect("deep parent");
    let deep = deep_parent.join("default");
    std::fs::rename(&lab.work, &deep).expect("move checkout under home");
    let workspace = deep_parent.join("feature");
    lab::jj(
        &deep,
        [
            "workspace",
            "add",
            "--name",
            "feature",
            workspace.to_str().expect("utf-8"),
        ],
    );
    let hidden = home.join(".cache").join("tool");
    jj_checkout(&hidden, &[("upstream", "https://forge.invalid/org/hidden")]);
    let too_deep = home.join("a").join("b").join("c").join("d");
    jj_checkout(
        &too_deep,
        &[("upstream", "https://forge.invalid/org/too-deep")],
    );
    let outside = lab.temp_path().join("outside");
    jj_checkout(
        &outside.join("linked"),
        &[("upstream", "https://forge.invalid/org/linked")],
    );
    std::os::unix::fs::symlink(&outside, home.join("link")).expect("symlink");

    let registry = registry(&[
        (
            "tool",
            entry(
                "https://forge.invalid/org/tool",
                "https://forge.invalid/acme/tool",
            ),
        ),
        (
            "plain",
            entry(
                "https://forge.invalid/org/plain",
                "https://forge.invalid/acme/plain",
            ),
        ),
        (
            "work",
            entry(
                lab.upstream.to_str().expect("utf-8"),
                "https://forge.invalid/acme/work.git",
            ),
        ),
        (
            "hidden",
            entry(
                "https://forge.invalid/org/hidden",
                "https://forge.invalid/acme/hidden",
            ),
        ),
        (
            "too-deep",
            entry(
                "https://forge.invalid/org/too-deep",
                "https://forge.invalid/acme/too-deep",
            ),
        ),
        (
            "linked",
            entry(
                "https://forge.invalid/org/linked",
                "https://forge.invalid/acme/linked",
            ),
        ),
    ]);

    let scan = bind::scan(&registry, &home);

    assert_eq!(
        scan.found
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["tool".to_owned(), "work".to_owned()],
        "problems: {:?}",
        scan.problems
    );
    assert_eq!(
        scan.found[&RepoName::new("work")].checkout.path,
        deep.canonicalize().expect("canonical")
    );
    assert_eq!(scan.home, home);
    assert!(scan.duplicates.is_empty(), "{:?}", scan.duplicates);
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
    // `plain` (git-only), `hidden` (dot-directory), `too-deep` (depth 4) and
    // `linked` (behind a symlink) are not found; the jj checkout is found once
    // although its workspace is under home too.
    std::fs::rename(&deep, &lab.work).expect("move checkout back for Lab's cleanup");
}

#[test]
fn scan_descends_through_a_git_tracked_parent() {
    // `~/work/.git` tracks the parent directory; the forks under it are still
    // the forks under it. Only a `.jj` stops the descent.
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    let parent = home.join("work");
    git_repository(
        &parent,
        &[("origin", "https://forge.invalid/someone/notes")],
    );
    let checkout = parent.join("tool");
    jj_checkout(&checkout, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[(
        "tool",
        entry(
            "https://forge.invalid/org/tool",
            "https://forge.invalid/acme/tool",
        ),
    )]);

    let scan = bind::scan(&registry, &home);

    assert_eq!(
        scan.found
            .get(&RepoName::new("tool"))
            .map(|fork| fork.checkout.path.clone()),
        Some(checkout.canonicalize().expect("canonical")),
        "problems: {:?}",
        scan.problems
    );
}

#[test]
fn scan_refuses_to_choose_between_two_checkouts_of_one_entry() {
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    let first = home.join("one");
    let second = home.join("two");
    jj_checkout(&first, &[("upstream", "https://forge.invalid/org/tool")]);
    jj_checkout(&second, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[(
        "tool",
        entry(
            "https://forge.invalid/org/tool",
            "https://forge.invalid/acme/tool",
        ),
    )]);

    let scan = bind::scan(&registry, &home);

    assert!(scan.found.is_empty());
    assert_eq!(
        scan.duplicates.get(&RepoName::new("tool")).map(Vec::len),
        Some(2)
    );
}

#[test]
fn scan_descends_from_a_home_that_is_itself_a_repository() {
    // A home directory kept as a dotfiles checkout holds `.git` at its root;
    // the forks under it are still the forks under it.
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    git_repository(
        &home,
        &[("origin", "https://forge.invalid/someone/dotfiles")],
    );
    let checkout = home.join("forks").join("tool").join("default");
    jj_checkout(&checkout, &[("upstream", "https://forge.invalid/org/tool")]);
    let registry = registry(&[(
        "tool",
        entry(
            "https://forge.invalid/org/tool",
            "https://forge.invalid/acme/tool",
        ),
    )]);

    let scan = bind::scan(&registry, &home);

    let found = scan
        .found
        .get(&RepoName::new("tool"))
        .expect("tool is found");
    assert_eq!(
        found.checkout.path,
        checkout.canonicalize().expect("checkout exists")
    );
    assert!(scan.duplicates.is_empty(), "{:?}", scan.duplicates);
    assert!(scan.problems.is_empty(), "{:?}", scan.problems);
}

#[test]
fn scan_names_a_checkout_whose_remotes_it_could_not_read() {
    // A `.jj/repo` directory with nothing in it looks like a checkout; jj
    // cannot read it. The scan says so rather than silently skipping what may
    // be the entry it was looking for.
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    let broken = home.join("broken");
    std::fs::create_dir_all(broken.join(".jj").join("repo")).expect("empty store");
    let registry = registry(&[(
        "ghost",
        entry(
            "https://forge.invalid/org/ghost",
            "https://forge.invalid/acme/ghost",
        ),
    )]);

    let scan = bind::scan(&registry, &home);

    assert_eq!(scan.problems.len(), 1, "{:?}", scan.problems);
    let canonical = broken.canonicalize().expect("canonical");
    assert!(
        scan.problems[0].starts_with(&format!("reading remotes of {}: ", canonical.display()))
            || scan.problems[0].starts_with(&format!("reading remotes of {}: ", broken.display())),
        "{:?}",
        scan.problems
    );
    // One line: jj's error, not its hints.
    assert!(!scan.problems[0].contains('\n'), "{:?}", scan.problems);
    let why = scan.unplaced(&RepoName::new("ghost"));
    assert!(
        why.message(&RepoName::new("ghost"), &registry)
            .contains("; could not read: reading remotes of"),
        "{why:?}"
    );
}

#[test]
fn resolve_prefers_the_bound_directory_then_the_scan_then_says_why_not() {
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let elsewhere = home.join("elsewhere");
    jj_checkout(
        &elsewhere,
        &[("upstream", "https://forge.invalid/org/elsewhere")],
    );
    let registry = registry(&[
        (
            "demo",
            entry(
                lab.upstream.to_str().expect("utf-8"),
                "https://forge.invalid/acme/work.git",
            ),
        ),
        (
            "elsewhere",
            entry(
                "https://forge.invalid/org/elsewhere",
                "https://forge.invalid/acme/elsewhere",
            ),
        ),
        (
            "absent",
            entry(
                "https://forge.invalid/org/absent",
                "https://forge.invalid/acme/absent",
            ),
        ),
    ]);

    let here = here_at(&registry, &lab.work);
    let demo =
        bind::resolve(&registry, &RepoName::new("demo"), here.clone(), &home).expect("resolved");
    assert_eq!(
        demo.checkout.path,
        lab.work.canonicalize().expect("canonical")
    );

    let other = bind::resolve(&registry, &RepoName::new("elsewhere"), here.clone(), &home)
        .expect("resolved");
    assert_eq!(
        other.checkout.path,
        elsewhere.canonicalize().expect("canonical")
    );

    let missing = bind::resolve(&registry, &RepoName::new("absent"), here.clone(), &home)
        .expect_err("missing");
    assert_eq!(
        missing,
        Unresolved::Missing {
            home: home.clone(),
            problems: Vec::new(),
        }
    );

    let unknown =
        bind::resolve(&registry, &RepoName::new("nope"), here, &home).expect_err("unknown");
    assert_eq!(unknown, Unresolved::Unknown);

    // Nothing bound: the scan is the only source.
    let scanned = bind::resolve(&registry, &RepoName::new("elsewhere"), None, &home)
        .expect("resolved from the scan");
    assert_eq!(scanned.checkout.path, other.checkout.path);
}

#[test]
fn a_repository_whose_remotes_cannot_be_read_is_an_error_naming_the_directory() {
    let lab = lab::Lab::new();
    let registry = registry(&[(
        "tool",
        entry(
            "https://forge.invalid/org/tool",
            "https://forge.invalid/acme/tool",
        ),
    )]);

    // A `.jj` with no repository inside: a root jj cannot read.
    let broken = lab.temp_path().join("broken");
    std::fs::create_dir_all(broken.join(".jj")).expect("broken .jj");
    let canonical = broken.canonicalize().expect("canonical");
    assert!(matches!(
        bind::here(&registry, &broken),
        Err(BindError::Remotes { root, .. }) if root == canonical
    ));

    // A workspace whose `.jj/repo` pointer cannot be read stays its own root,
    // so the error names the directory the user stands in.
    let stray = lab.temp_path().join("stray");
    std::fs::create_dir_all(stray.join(".jj")).expect("stray .jj");
    std::fs::write(stray.join(".jj").join("repo"), [0xff, 0xfe]).expect("unreadable pointer");
    let canonical = stray.canonicalize().expect("canonical");
    assert_eq!(bind::checkout_root(&stray), Some(canonical.clone()));
    assert!(matches!(
        bind::here(&registry, &stray),
        Err(BindError::Remotes { root, .. }) if root == canonical
    ));
}

#[test]
fn a_workspace_whose_checkout_was_deleted_reports_jj_s_own_error_about_the_workspace() {
    // The pointer names a store that is gone. The workspace is a repository
    // directory jj cannot open — not a directory that is no repository at all.
    let lab = lab::Lab::new();
    let registry = registry(&[(
        "demo",
        entry(
            lab.upstream.to_str().expect("utf-8"),
            "https://forge.invalid/acme/work.git",
        ),
    )]);
    let checkout = lab.temp_path().join("gone").join("default");
    jj_checkout(&checkout, &[("upstream", "https://forge.invalid/org/gone")]);
    let workspace = lab.temp_path().join("gone").join("feature");
    lab::jj(
        &checkout,
        [
            "workspace",
            "add",
            "--name",
            "feature",
            workspace.to_str().expect("utf-8"),
        ],
    );
    std::fs::remove_dir_all(&checkout).expect("delete the checkout under the workspace");
    let canonical = workspace.canonicalize().expect("canonical");

    assert_eq!(bind::checkout_root(&workspace), Some(canonical.clone()));
    let error = bind::here(&registry, &workspace).expect_err("unreadable");
    assert!(
        matches!(&error, BindError::Remotes { root, .. } if *root == canonical),
        "{error:?}"
    );
    assert!(
        !error
            .to_string()
            .contains("neither a jj nor a git repository"),
        "{error}"
    );

    // Through the binary: jj's words about the workspace, exit 3, no sweep.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n",
            lab.upstream.display()
        ),
    )
    .expect("registry");
    let output = lab::knives_command(
        &workspace,
        home.path(),
        lab.temp_path(),
        &["--text", "status", "--no-landed", "--no-github"],
    )
    .output()
    .expect("run knives");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!("reading remotes of {}: ", canonical.display())),
        "{stderr}"
    );
    assert!(stderr.contains("Cannot access"), "{stderr}");
    assert!(
        !stderr.contains("neither a jj nor a git repository"),
        "{stderr}"
    );
}

fn knives(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    lab::knives_command(&lab.work, home.path(), lab.temp_path(), args)
        .output()
        .expect("run knives")
}

fn knives_outside(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let outside = lab.temp_path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    lab::knives_command(&outside, home.path(), lab.temp_path(), args)
        .output()
        .expect("run knives")
}

/// The lab's registry with a second entry, `ghost`, that has no checkout anywhere.
fn home_with_a_ghost(lab: &lab::Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let (home, consumer) = lab::release_test_home(lab);
    let path = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&path).expect("registry");
    text.push_str(
        "\n[repos.ghost]\nupstream = \"https://forge.invalid/org/ghost\"\n\
         origin = \"https://forge.invalid/acme/ghost\"\n",
    );
    std::fs::write(&path, text).expect("registry");
    (home, consumer)
}

#[test]
fn a_named_verb_whose_checkout_is_not_on_this_machine_exits_usage_and_says_so() {
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.ghost]\nupstream = \"https://forge.invalid/org/ghost\"\norigin = \"https://forge.invalid/acme/ghost\"\n",
    )
    .expect("registry");
    let output = knives(&lab, &home, &["--text", "notch", "--repo", "ghost"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no checkout of ghost under"), "{stderr}");
    assert!(!stderr.contains("known:"), "{stderr}");
}

#[test]
fn a_named_verb_names_what_the_scan_could_not_read_beside_the_missing_checkout() {
    // A broken checkout under home may be the one the entry wanted; the
    // refusal says so instead of dropping it.
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.ghost]\nupstream = \"https://forge.invalid/org/ghost\"\norigin = \"https://forge.invalid/acme/ghost\"\n",
    )
    .expect("registry");
    let broken = lab.temp_path().join("broken");
    std::fs::create_dir_all(broken.join(".jj").join("repo")).expect("empty store");
    let output = lab::knives_command(
        lab.temp_path(),
        home.path(),
        lab.temp_path(),
        &["--text", "notch", "--repo", "ghost"],
    )
    .output()
    .expect("run knives");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no checkout of ghost under"), "{stderr}");
    assert!(
        stderr.contains("; could not read: reading remotes of"),
        "{stderr}"
    );
    assert!(stderr.contains("broken"), "{stderr}");
}

#[test]
fn status_inside_a_bound_checkout_reports_only_it_and_carries_the_origin_note() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let output = knives(
        &lab,
        &home,
        &["--text", "status", "--no-landed", "--no-github"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo"), "{stdout}");
    // The lab's checkout origin is a local bare path; the registry says a forge URL.
    assert!(stdout.contains("origin remote is "), "{stdout}");
    assert!(
        stdout.contains("; registry says https://forge.invalid/acme/work.git"),
        "{stdout}"
    );
}

#[test]
fn status_inside_a_registered_git_only_clone_refuses_as_every_fork_verb_does() {
    // The hook binds git clones; fork verbs need jj. Standing in one, `status`
    // says so rather than sweeping or opening it.
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let clone = lab.temp_path().join("clone");
    git_repository(
        &clone,
        &[("upstream", lab.upstream.to_str().expect("utf-8"))],
    );
    let output = lab::knives_command(
        &clone,
        home.path(),
        lab.temp_path(),
        &["--text", "status", "--no-landed", "--no-github"],
    )
    .output()
    .expect("run knives");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("is a git clone, not a jj checkout; fork commands need jj"),
        "{stderr}"
    );
}

#[test]
fn a_named_status_from_inside_its_checkout_reads_the_remotes_once() {
    // `Ground` binds the current directory once; `resolve` is handed that
    // binding rather than asking jj a second time. Counted through a `jj`
    // shim on PATH that logs every `git remote list` before delegating.
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let shim_dir = lab.temp_path().join("shim");
    std::fs::create_dir_all(&shim_dir).expect("shim dir");
    let log = lab.temp_path().join("jj-calls.log");
    let real_jj = which_jj();
    let shim = shim_dir.join("jj");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \"$*\" in *\"git remote list\"*) echo \"$*\" >> '{}';; esac\nexec '{}' \"$@\"\n",
            log.display(),
            real_jj.display()
        ),
    )
    .expect("write shim");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let path = format!(
        "{}:{}",
        shim_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = lab::knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &["--text", "status", "demo", "--no-landed", "--no-github"],
    )
    .env("PATH", path)
    .output()
    .expect("run knives");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        calls.lines().count(),
        1,
        "jj git remote list was run more than once:\n{calls}"
    );
}

/// The `jj` the tests otherwise run, so a shim can delegate to it.
fn which_jj() -> std::path::PathBuf {
    let output = std::process::Command::new("sh")
        .args(["-c", "command -v jj"])
        .output()
        .expect("locate jj");
    std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

#[test]
fn sync_outside_any_checkout_without_a_name_or_all_exits_usage() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let output = knives_outside(&lab, &home, &["--text", "sync", "--no-github"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("give a repo name, or --all"));
}

#[test]
fn status_outside_any_checkout_sweeps_every_entry_through_the_scan() {
    let lab = lab::Lab::new();
    let (home, _consumer) = home_with_a_ghost(&lab);
    let output = knives_outside(
        &lab,
        &home,
        &["--text", "status", "--no-landed", "--no-github"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(stdout.contains("demo"), "{stdout}\n{stderr}");
    // The scan placed the checkout: the row was gathered, not refused, and
    // carries the checkout's origin note.
    assert!(
        stdout.contains("origin remote is "),
        "a gathered row carries the checkout's origin note:\n{stdout}\n{stderr}"
    );
    // The entry with no checkout is still a row, refused.
    assert!(stdout.contains("ghost"), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("could not gather: no checkout of ghost under"),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        stdout.matches("could not gather").count(),
        1,
        "only the ghost row is refused:\n{stdout}"
    );
}

#[test]
fn status_json_outside_any_checkout_carries_the_unplaced_row_as_a_problem() {
    let lab = lab::Lab::new();
    let (home, _consumer) = home_with_a_ghost(&lab);
    let output = knives_outside(
        &lab,
        &home,
        &["--json", "status", "--no-landed", "--no-github"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    let reports: serde_json::Value = serde_json::from_str(&stdout).expect("json document");
    let reports = reports.as_array().expect("one document per repository");
    let ghost = reports
        .iter()
        .find(|report| report["repo"] == "ghost")
        .expect("a ghost row");
    let problems = ghost["problems"].as_array().expect("problems array");
    assert_eq!(problems.len(), 1, "{ghost}");
    assert!(
        problems[0]
            .as_str()
            .expect("text")
            .contains("could not gather: no checkout of ghost under"),
        "{ghost}"
    );
    let demo = reports
        .iter()
        .find(|report| report["repo"] == "demo")
        .expect("a demo row");
    assert!(
        demo["problems"].as_array().is_none_or(Vec::is_empty),
        "{demo}"
    );
}

#[test]
fn sync_all_outside_any_checkout_reports_the_unplaced_entry_and_syncs_the_rest() {
    let lab = lab::Lab::new();
    let (home, _consumer) = home_with_a_ghost(&lab);
    let output = knives_outside(&lab, &home, &["--text", "sync", "--all", "--no-github"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(stdout.contains("demo"), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("could not gather: no checkout of ghost under"),
        "{stdout}\n{stderr}"
    );
}

#[test]
fn a_sweep_says_once_what_the_scan_could_not_read_even_when_every_entry_is_found() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let broken = lab.temp_path().join("broken");
    std::fs::create_dir_all(broken.join(".jj").join("repo")).expect("empty store");
    let output = knives_outside(
        &lab,
        &home,
        &["--json", "status", "--all", "--no-landed", "--no-github"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("demo"), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("could not read: reading remotes of"),
        "{stderr}"
    );
    assert!(stderr.contains("broken"), "{stderr}");
    assert_eq!(stderr.matches("could not read").count(), 1, "{stderr}");
    // The document is still the document.
    assert!(!stdout.contains("could not read"), "{stdout}");
}
