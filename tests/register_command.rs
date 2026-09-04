//! `knives register` through the real binary: a checkout the registry does not
//! list gets a paste-ready entry without `path`; one it does list is named,
//! from any directory inside it; one whose name another entry already holds is
//! refused.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

fn knives(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    lab::knives_command(&lab.work, home.path(), lab.temp_path(), args)
        .output()
        .expect("run knives")
}

#[test]
fn register_prints_a_snippet_without_path_for_an_unregistered_checkout() {
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "").expect("empty registry");
    let output = knives(&lab, &home, &["--text", "register"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("[repos.work]"), "{stdout}");
    assert!(
        stdout.contains(&format!("upstream = \"{}\"", lab.upstream.display())),
        "{stdout}"
    );
    assert!(!stdout.contains("path ="), "{stdout}");
    // Under identity binding an entry is never replaced by a same-named one.
    assert!(!stdout.contains("replace"), "{stdout}");
    assert!(!stderr.contains("replace"), "{stderr}");
    assert!(stderr.contains("paste this into"), "{stderr}");
}

#[test]
fn register_names_the_entry_a_registered_checkout_already_is_from_any_subdirectory() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let nested = lab.work.join("src");
    std::fs::create_dir_all(&nested).expect("nested");
    let output = lab::knives_command(
        &nested,
        home.path(),
        lab.temp_path(),
        &["--text", "register"],
    )
    .output()
    .expect("run knives");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "already registered as demo"
    );
}

#[test]
fn register_refuses_a_checkout_whose_name_another_repository_s_entry_holds() {
    // The lab's checkout is `work`; the registry already has `[repos.work]`
    // for a different upstream. Identity is the upstream, so that entry is
    // not this checkout's to replace.
    let lab = lab::Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.work]\nupstream = \"https://forge.invalid/other/work\"\norigin = \"https://forge.invalid/acme/work\"\n",
    )
    .expect("registry");
    let output = knives(&lab, &home, &["--text", "register"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stdout}\n{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains(
            "[repos.work] already names https://forge.invalid/other/work; pick another name, or \
             update that entry's upstream if this is the same repository renamed"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("replace"), "{stderr}");
}
