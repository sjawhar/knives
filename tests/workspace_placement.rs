//! Where a branch's workspace goes when the registry entry says where.
//!
//! The `<name>/default` layout puts each branch's workspace beside `default`. A
//! checkout at `~/<name>` has no such room: siblings would land in `~` itself, one
//! directory per branch across every repository. An entry's `workspaces` names
//! the directory instead, and `start` and `finish` both honour it.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::Lab;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A registry naming a workspaces directory for the lab checkout, and that directory.
fn home_with_workspaces(lab: &Lab) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("create config home");
    let workspaces = home.path().join("worktrees").join("demo");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\n\
             origin = \"https://forge.invalid/acme/work.git\"\nworkspaces = \"{}\"\n",
            lab.work.display(),
            lab.upstream.display(),
            workspaces.display(),
        ),
    )
    .expect("write registry");
    (home, workspaces)
}

fn knives(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(args)
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run knives")
}

/// Start `feat/gamma` into the configured directory; the fixture every test shares.
fn start_gamma(lab: &Lab, home: &tempfile::TempDir) -> std::process::Output {
    knives(
        lab,
        home,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    )
}

/// Whether `directory` is `checkout`'s workspace named `name`.
fn is_workspace_named(directory: &Path, checkout: &Path, name: &str) -> bool {
    knives::jj::is_workspace_named(directory, checkout, &knives::ids::WorkspaceName::new(name))
}

/// The workspace names the checkout's repository lists.
fn workspaces_of(checkout: &Path) -> Vec<String> {
    knives::jj::Repo::open(checkout)
        .expect("open checkout")
        .workspaces()
        .expect("list workspaces")
        .into_iter()
        .map(|(name, _)| name.to_string())
        .collect()
}

#[test]
fn start_opens_the_workspace_under_the_configured_directory() {
    // Given: an entry whose workspaces directory does not exist yet
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);

    // When: a branch is started
    let started = start_gamma(&lab, &home);

    // Then: the workspace is there, attached to the checkout, and nothing landed beside it
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let workspace = workspaces.join("feat-gamma");
    assert!(
        is_workspace_named(&workspace, &lab.work, "feat-gamma"),
        "{} is not the checkout's workspace feat-gamma",
        workspace.display()
    );
    assert_eq!(
        workspaces_of(&lab.work),
        vec!["default".to_owned(), "feat-gamma".to_owned()]
    );
    assert!(
        !lab.work
            .parent()
            .expect("checkout parent")
            .join("feat-gamma")
            .exists(),
        "a sibling workspace was created despite the configured directory"
    );
    let stdout = String::from_utf8_lossy(&started.stdout);
    assert!(
        stdout.contains(&workspace.display().to_string()),
        "start did not name the workspace it opened: {stdout}"
    );
}

#[test]
fn finish_removes_the_workspace_from_the_configured_directory() {
    // Given: a branch started into the configured directory
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    let started = start_gamma(&lab, &home);
    assert!(started.status.success(), "{started:?}");
    let workspace = workspaces.join("feat-gamma");
    assert!(workspace.is_dir(), "fixture: workspace not created");

    // When: the branch is finished
    let finished = knives(
        &lab,
        &home,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the directory is gone and the workspace is no longer registered
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(
        !workspace.exists(),
        "finish left {} on disk",
        workspace.display()
    );
    assert_eq!(
        workspaces_of(&lab.work),
        vec!["default".to_owned()],
        "the workspace is still registered"
    );
}

#[test]
fn a_measurement_workspace_opens_under_the_workspace_root_and_leaves_nothing_behind() {
    // Given: a checkout and a workspace root that does not exist yet
    let lab = Lab::new();
    let scratch = tempfile::tempdir().expect("create scratch home");
    let root = scratch.path().join("worktrees").join("demo");

    // When: a command is measured at a revision
    let output = knives::jj::output_at_revision(&lab.work, &root, "main@upstream", "pwd -P")
        .expect("measure at revision");

    // Then: it ran under the root and the workspace is gone. The root itself
    // survives: knives created it as the destination's parent, and only the
    // measurement directory under it is removed.
    let ran_in = PathBuf::from(output.trim());
    assert!(
        ran_in.starts_with(root.canonicalize().expect("canonical root")),
        "ran in {} rather than under {}",
        ran_in.display(),
        root.display()
    );
    assert!(
        !ran_in.exists(),
        "measurement workspace {} was left behind",
        ran_in.display()
    );
}

#[test]
fn finish_leaves_a_directory_that_is_not_a_workspace_of_the_checkout_alone() {
    // Given: something else at the path a branch's workspace would take. With
    // `workspaces` free to point anywhere, `jj workspace forget` exiting 0 for an
    // unknown name is all that stood between `finish` and `remove_dir_all`.
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    let bystander = workspaces.join("feat-gamma");
    std::fs::create_dir_all(&bystander).expect("create bystander");
    std::fs::write(bystander.join("notes.txt"), "keep\n").expect("write bystander file");

    // When: the never-started branch is finished
    let finished = knives(
        &lab,
        &home,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the directory and its contents survive, and the output says why
    assert!(finished.status.success(), "{finished:?}");
    assert!(
        bystander.join("notes.txt").is_file(),
        "finish removed a directory that was never a workspace of the checkout"
    );
    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        stdout.contains("not a workspace of") && !stdout.contains("forgotten"),
        "finish misdescribed a directory that was never its workspace: {stdout}"
    );
}

#[test]
fn finish_says_when_there_is_no_directory_to_remove() {
    // Given: a started branch whose workspace directory was removed by hand
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    assert!(start_gamma(&lab, &home).status.success());
    let workspace = workspaces.join("feat-gamma");
    std::fs::remove_dir_all(&workspace).expect("remove workspace by hand");

    // When: the branch is finished
    let finished = knives(
        &lab,
        &home,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the registration is gone and the output does not claim a directory was left
    assert!(finished.status.success(), "{finished:?}");
    assert_eq!(workspaces_of(&lab.work), vec!["default".to_owned()]);
    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        stdout.contains("no directory at") && !stdout.contains("left on disk"),
        "finish described a directory that does not exist: {stdout}"
    );
}

#[test]
fn start_refuses_to_adopt_a_directory_that_is_not_a_workspace_of_the_checkout() {
    // Given: a plain directory where the branch's workspace would go
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    let bystander = workspaces.join("feat-gamma");
    std::fs::create_dir_all(&bystander).expect("create bystander");
    std::fs::write(bystander.join("notes.txt"), "keep\n").expect("write bystander file");

    // When: the branch is started
    let started = start_gamma(&lab, &home);

    // Then: nothing is claimed or adopted, and the directory is untouched
    assert_eq!(started.status.code(), Some(2), "{started:?}");
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(stderr.contains("cannot adopt"), "was: {stderr}");
    assert!(bystander.join("notes.txt").is_file());
    assert!(
        !home.path().join("state.json").exists(),
        "a claim was recorded for a workspace that was never adopted"
    );
}

#[test]
fn finish_from_inside_the_configured_workspace_releases_by_possession() {
    // Given: a branch started by one owner into the configured directory
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    assert!(start_gamma(&lab, &home).status.success());
    let workspace = workspaces.join("feat-gamma");

    // When: another owner finishes it from inside, naming no repository
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma"])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "someone-else")
        .output()
        .expect("run finish from inside");

    // Then: the repository is inferred through the pointer and possession releases the claim
    assert!(
        finished.status.success(),
        "finish from inside failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!workspace.exists(), "the workspace directory survived");
    assert_eq!(workspaces_of(&lab.work), vec!["default".to_owned()]);
}

#[test]
fn a_cut_measures_tests_under_the_configured_workspaces_directory() {
    // Given: two branches, a workspaces directory, and a test counter that records
    // where it ran. The measurement workspace used to open beside the checkout,
    // which for a `~/<name>` checkout is `~`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, workspaces) = home_with_workspaces(&lab);
    let measured = home.path().join("measured");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\n\
             origin = \"https://forge.invalid/acme/work.git\"\nworkspaces = \"{}\"\n\
             test_count_command = \"pwd -P >> {}; printf 10\"\n",
            lab.work.display(),
            lab.upstream.display(),
            workspaces.display(),
            measured.display(),
        ),
    )
    .expect("configure test counter");

    // When: a release is cut
    let cut = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-05",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run cut");

    // Then: both measurements ran under the configured directory
    assert!(
        cut.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&cut.stdout),
        String::from_utf8_lossy(&cut.stderr)
    );
    let root = workspaces.canonicalize().expect("canonical workspaces");
    let ran_in: Vec<PathBuf> = std::fs::read_to_string(&measured)
        .expect("read measurements")
        .lines()
        .map(PathBuf::from)
        .collect();
    assert_eq!(ran_in.len(), 2, "was: {ran_in:?}");
    assert!(
        ran_in.iter().all(|directory| directory.starts_with(&root)),
        "a measurement ran outside {}: {ran_in:?}",
        root.display()
    );
}

#[test]
fn finish_leaves_a_workspace_of_the_checkout_registered_under_another_name_alone() {
    // Given: this checkout's workspace `other` sitting where feat/gamma's would go.
    // Store identity alone would call it ours and remove a live workspace.
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    let occupied = workspaces.join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "other", &occupied, "main@upstream")
        .expect("open workspace `other` at feat/gamma's path");

    // When: feat/gamma is finished
    let finished = knives(
        &lab,
        &home,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: `other` survives on disk and in the repository, and the output names it
    assert!(finished.status.success(), "{finished:?}");
    assert!(
        occupied.join(".jj").is_dir(),
        "finish removed workspace `other`"
    );
    assert_eq!(
        workspaces_of(&lab.work),
        vec!["default".to_owned(), "other".to_owned()]
    );
    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        stdout.contains("other") && stdout.contains("left alone") && !stdout.contains("forgotten"),
        "finish misdescribed a workspace registered under another name: {stdout}"
    );
}

#[test]
fn start_refuses_to_adopt_a_plain_directory_even_when_the_name_is_registered() {
    // Given: feat/gamma's workspace registered, its directory replaced by hand with
    // something that is not a workspace, and no claim left to resume
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    assert!(start_gamma(&lab, &home).status.success());
    let workspace = workspaces.join("feat-gamma");
    std::fs::remove_dir_all(&workspace).expect("remove the workspace by hand");
    std::fs::create_dir_all(&workspace).expect("put a plain directory in its place");
    std::fs::write(workspace.join("notes.txt"), "keep\n").expect("write bystander file");
    std::fs::remove_file(home.path().join("state.json")).expect("drop the claim");

    // When: the branch is started again
    let started = start_gamma(&lab, &home);

    // Then: the registered name does not make the directory adoptable
    assert_eq!(started.status.code(), Some(2), "{started:?}");
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("cannot adopt"),
        "{started:?}"
    );
    assert!(workspace.join("notes.txt").is_file());
}

#[test]
fn finish_by_possession_sees_through_a_symlinked_workspaces_directory() {
    // Given: `workspaces` spelled through a symlink, as a directory on another disk
    // would be. The shell's cwd is the physical path; the registry's is not.
    let lab = Lab::new();
    let home = tempfile::tempdir().expect("create config home");
    let physical = home.path().join("disk").join("worktrees");
    std::fs::create_dir_all(&physical).expect("create physical worktrees");
    let link = home.path().join("worktrees");
    std::os::unix::fs::symlink(&physical, &link).expect("symlink worktrees");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\n\
             origin = \"https://forge.invalid/acme/work.git\"\nworkspaces = \"{}\"\n",
            lab.work.display(),
            lab.upstream.display(),
            link.join("demo").display(),
        ),
    )
    .expect("write registry");
    assert!(start_gamma(&lab, &home).status.success());
    let workspace = physical.join("demo").join("feat-gamma");
    assert!(
        workspace.is_dir(),
        "fixture: workspace not created through the link"
    );

    // When: another owner finishes from inside, standing on the physical path
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma", "--repo", "demo"])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "someone-else")
        .output()
        .expect("run finish from inside");

    // Then: possession is recognised and the claim released
    assert!(
        finished.status.success(),
        "possession through a symlink was not recognised: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!workspace.exists());
}

#[test]
fn finish_leaves_the_directory_when_the_registration_could_not_be_forgotten() {
    // Given: a started branch, and a `jj` whose `workspace forget` fails (a lock
    // held elsewhere, a broken store). Removing the directory anyway leaves a
    // registration nothing can rebuild: every later `start` dies on jj's
    // "Workspace named … already exists", `--force` included.
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    assert!(start_gamma(&lab, &home).status.success());
    let workspace = workspaces.join("feat-gamma");
    let real_jj = which_jj();
    let shim = tempfile::tempdir().expect("create jj shim directory");
    let jj = shim.path().join("jj");
    std::fs::write(
        &jj,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = forget ] && \
             {{ echo 'simulated lock failure' >&2; exit 1; }}; done\nexec {} \"$@\"\n",
            real_jj.display()
        ),
    )
    .expect("write jj shim");
    let mut permissions = std::fs::metadata(&jj).expect("shim metadata").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&jj, permissions).expect("chmod shim");
    let path = std::env::join_paths(std::iter::once(shim.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set")),
    ))
    .expect("construct shim PATH");

    // When: the branch is finished
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "ses_fff688")
        .env("PATH", path)
        .output()
        .expect("run finish with a failing jj");

    // Then: the claim is released, the failure is reported, and the directory
    // stays so a retry can finish the job
    assert!(finished.status.success(), "{finished:?}");
    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        stdout.contains("not forgotten") && stdout.contains("left on disk"),
        "finish did not report the failed forget and the retained directory: {stdout}"
    );
    assert!(
        workspace.join(".jj").is_dir(),
        "finish removed the directory of a registration it could not forget"
    );
    assert_eq!(
        workspaces_of(&lab.work),
        vec!["default".to_owned(), "feat-gamma".to_owned()]
    );
}

fn which_jj() -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", "command -v jj"])
        .output()
        .expect("locate jj");
    PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
}

#[test]
fn start_refuses_a_branch_named_for_the_primary_workspace_under_a_configured_directory() {
    // Given: a configured directory, where `default` no longer maps to the
    // checkout itself but to `<dir>/default`. Nothing sits there, so without the
    // shared collision rule `start` reaches `jj workspace add --name default`
    // and dies on jj's "already exists".
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);

    // When: the branch `default` is started
    let started = knives(
        &lab,
        &home,
        &["--text", "start", "default", "--repo", "demo", "--why", "x"],
    );

    // Then: refused as the checkout collision it is, with nothing claimed or created
    assert_eq!(started.status.code(), Some(2), "{started:?}");
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(stderr.contains("registered checkout"), "was: {stderr}");
    assert!(!workspaces.join("default").exists());
    assert!(!home.path().join("state.json").exists());
}

#[test]
fn finish_leaves_a_symbolic_link_at_the_workspace_path_alone() {
    // Given: a real workspace of this branch elsewhere, and a symbolic link to it
    // at the derived path. `remove_dir_all` unlinks a symlink rather than
    // following it; reporting that as "removed" would hide a live directory
    // whose registration is gone.
    let lab = Lab::new();
    let (home, workspaces) = home_with_workspaces(&lab);
    let elsewhere = home.path().join("elsewhere").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &elsewhere, "main@upstream")
        .expect("open the real workspace elsewhere");
    std::fs::create_dir_all(&workspaces).expect("create workspaces directory");
    std::os::unix::fs::symlink(&elsewhere, workspaces.join("feat-gamma")).expect("link");

    // When: the branch is finished
    let finished = knives(
        &lab,
        &home,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the link and its target survive, and the output says what it found
    assert!(finished.status.success(), "{finished:?}");
    assert!(
        std::fs::symlink_metadata(workspaces.join("feat-gamma"))
            .expect("link metadata")
            .file_type()
            .is_symlink(),
        "finish removed the link"
    );
    assert!(
        elsewhere.join(".jj").is_dir(),
        "finish removed the link's target"
    );
    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        stdout.contains("symbolic link") && !stdout.contains("removed"),
        "finish misdescribed a symbolic link: {stdout}"
    );
}
