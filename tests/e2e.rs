use std::{
    ffi::OsStr,
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct TestDaemon<'a> {
    binary: &'a str,
    runtime: std::path::PathBuf,
    state: std::path::PathBuf,
    config: std::path::PathBuf,
}

impl TestDaemon<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new(self.binary);
        command.envs([
            ("XDG_RUNTIME_DIR", self.runtime.as_os_str()),
            ("XDG_STATE_HOME", self.state.as_os_str()),
            ("XDG_CONFIG_HOME", self.config.as_os_str()),
        ]);
        command
    }

    fn wait_for(&self, needle: &str, pane_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self
                .command()
                .args(["capture", "--pane", pane_id, "--lines", "20"])
                .output()
                .unwrap();
            if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(needle) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("pane never emitted {needle:?}");
    }
}

impl Drop for TestDaemon<'_> {
    fn drop(&mut self) {
        let _ = self.command().args(["server", "stop", "--force"]).output();
    }
}

#[test]
fn detached_pty_input_lifecycle_and_capture_work() {
    let binary = env!("CARGO_BIN_EXE_muxloom");
    let temporary = tempfile::tempdir().unwrap();
    let runtime = temporary.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let daemon = TestDaemon {
        binary,
        runtime,
        state: temporary.path().join("state"),
        config: temporary.path().join("config"),
    };

    let created = daemon
        .command()
        .args([
            "new",
            "--detach",
            "--name",
            "e2e",
            "--",
            "/bin/sh",
            "-lc",
            "printf 'e2e-ready\\n'; read line; printf 'got:%s\\n' \"$line\"; sleep 10",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );

    let listed = daemon.command().args(["list", "--json"]).output().unwrap();
    assert!(listed.status.success());
    let workspaces: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let pane_id = workspaces[0]["panes"][0]["id"].as_str().unwrap().to_owned();
    daemon.wait_for("e2e-ready", &pane_id);

    let sent = daemon
        .command()
        .args(["send", "--pane", &pane_id, "--enter", "hello"])
        .output()
        .unwrap();
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    daemon.wait_for("got:hello", &pane_id);

    let renamed = daemon
        .command()
        .args(["workspace", "rename", "e2e", "renamed"])
        .output()
        .unwrap();
    assert!(renamed.status.success());
    let killed = daemon
        .command()
        .args(["workspace", "kill", "renamed", "--force"])
        .output()
        .unwrap();
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
}

#[test]
fn passive_hook_does_not_start_a_stopped_daemon() {
    let binary = env!("CARGO_BIN_EXE_muxloom");
    let temporary = tempfile::tempdir().unwrap();
    let runtime = temporary.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let output = Command::new(binary)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", temporary.path().join("state"))
        .env("XDG_CONFIG_HOME", temporary.path().join("config"))
        .env("MUXLOOM_PANE_ID", OsStr::new("missing"))
        .args(["hook", "--provider", "codex", "--state", "working"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!runtime.join("muxloom/muxloom.sock").exists());
}
