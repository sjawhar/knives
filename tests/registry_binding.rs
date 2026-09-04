#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! A checkout is bound to its registry entry by its `upstream` remote, from the
//! directory you stand in or by scanning `$HOME`.

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knives::bind::{self, BindError, Unbound, Unresolved};
use knives::config::{Registry, RepoEntry};
use knives::ids::RepoName;

fn entry(upstream: &str, origin: &str) -> RepoEntry {
    RepoEntry {
        path: PathBuf::from("/unused"), // Task 3 deletes this field and this line
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

/// A jj checkout (colocated) with the given remotes: what the scan looks for.
fn jj_checkout(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create checkout");
    let jj = |args: &[&str]| {
        let status = std::process::Command::new("jj")
            .args(args)
            .current_dir(root)
            .env("JJ_CONFIG", "/dev/null")
            .env("JJ_USER", "Knives Lab")
            .env("JJ_EMAIL", "knives-lab@example.test")
            .status()
            .expect("run jj");
        assert!(status.success(), "jj {args:?} failed");
    };
    jj(&["git", "init", "--colocate"]);
    for (name, url) in remotes {
        jj(&["git", "remote", "add", name, url]);
    }
}

/// A git-only repository with the given remotes, the shape an agent's `/tmp` clone has.
fn git_repository(root: &Path, remotes: &[(&str, &str)]) {
    std::fs::create_dir_all(root).expect("create repository");
    let init = std::process::Command::new("git")
        .args(["-C", root.to_str().expect("utf-8"), "init", "--quiet"])
        .status()
        .expect("git init");
    assert!(init.success());
    for (name, url) in remotes {
        let added = std::process::Command::new("git")
            .args([
                "-C",
                root.to_str().expect("utf-8"),
                "remote",
                "add",
                name,
                url,
            ])
            .status()
            .expect("git remote add");
        assert!(added.success());
    }
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
    assert_eq!(bind::checkout_root(&workspace), Some(expected));
    // The workspace is its own nearest root; the checkout is the subdirectory's.
    assert_eq!(
        bind::nearest_root(&workspace),
        Some(workspace.canonicalize().expect("canonical ws"))
    );
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
    let git_remotes = bind::remotes(&clone).expect("git remotes");
    assert_eq!(
        git_remotes,
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
fn scan_finds_each_entry_once_skips_workspaces_and_dot_directories_and_stops_at_depth_three() {
    // Given: a home with a checkout at depth 1, one at depth 3, a workspace, a
    // dot-directory hiding a checkout, a checkout at depth 4, and a git-only
    // clone whose upstream matches an entry.
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
    assert!(scan.duplicates.is_empty(), "{:?}", scan.duplicates);
    // `plain` (git-only), `hidden` (dot-directory) and `too-deep` (depth 4) are
    // not found; the jj checkout is found once although its workspace is under home too.
    std::fs::rename(&deep, &lab.work).expect("move checkout back for Lab's cleanup");
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
fn resolve_prefers_the_current_directory_then_the_scan_then_says_why_not() {
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

    let demo = bind::resolve(&registry, &RepoName::new("demo"), &lab.work, &home)
        .expect("read")
        .expect("resolved");
    assert_eq!(
        demo.checkout.path,
        lab.work.canonicalize().expect("canonical")
    );

    let other = bind::resolve(&registry, &RepoName::new("elsewhere"), &lab.work, &home)
        .expect("read")
        .expect("resolved");
    assert_eq!(
        other.checkout.path,
        elsewhere.canonicalize().expect("canonical")
    );

    let missing = bind::resolve(&registry, &RepoName::new("absent"), &lab.work, &home)
        .expect("read")
        .expect_err("missing");
    assert_eq!(missing, Unresolved::Missing { home: home.clone() });

    let unknown = bind::resolve(&registry, &RepoName::new("nope"), &lab.work, &home)
        .expect("read")
        .expect_err("unknown");
    assert_eq!(unknown, Unresolved::Unknown);
}

#[test]
fn a_repository_whose_remotes_cannot_be_read_is_an_error_not_a_reason_to_scan() {
    let lab = lab::Lab::new();
    let home = lab.temp_path().join("home");
    jj_checkout(
        &home.join("tool"),
        &[("upstream", "https://forge.invalid/org/tool")],
    );
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
    // The scan would find `tool` under home; the cwd's own error comes first.
    assert!(matches!(
        bind::resolve(&registry, &RepoName::new("tool"), &broken, &home),
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
