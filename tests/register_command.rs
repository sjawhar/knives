//! `knives register` through the real binary: a checkout the registry does not
//! list gets a paste-ready entry without `path`; one it does list is named,
//! from any directory inside it.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

fn knives(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(args)
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
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
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("[repos.work]"), "{stdout}");
    assert!(
        stdout.contains(&format!("upstream = \"{}\"", lab.upstream.display())),
        "{stdout}"
    );
    assert!(!stdout.contains("path ="), "{stdout}");
}

#[test]
fn register_names_the_entry_a_registered_checkout_already_is_from_any_subdirectory() {
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let nested = lab.work.join("src");
    std::fs::create_dir_all(&nested).expect("nested");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "register"])
        .current_dir(&nested)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run knives");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "already registered as demo"
    );
}
