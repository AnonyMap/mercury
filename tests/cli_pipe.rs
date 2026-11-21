//! The terminal commands end quietly when their output pipe closes (`mercury sessions | head`),
//! instead of panicking on "Broken pipe".

use std::process::{Command, Stdio};

#[test]
fn closed_stdout_is_not_a_crash() {
    let home = tempfile::tempdir().unwrap();
    for args in [&["presets"][..], &["profile", "list"][..], &["sessions"][..]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mercury"))
            .args(args)
            .env("MERCURY_HOME", home.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Close the reading end before the program writes anything.
        drop(child.stdout.take());
        let out = child.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{args:?}: {err}");
        assert!(out.status.success(), "{args:?}: {:?} {err}", out.status);
    }
    assert!(!home.path().join("logs/last-crash.txt").exists(), "no crash report either");
}
