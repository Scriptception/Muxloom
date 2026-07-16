use std::{process::Command, thread, time::Duration};

#[test]
fn detached_pty_can_be_listed_and_captured() {
    let binary = env!("CARGO_BIN_EXE_muxloom");
    let temporary = tempfile::tempdir().unwrap();
    let runtime = temporary.path().join("runtime");
    let state = temporary.path().join("state");
    let config = temporary.path().join("config");
    std::fs::create_dir_all(&runtime).unwrap();
    let environment = [
        ("XDG_RUNTIME_DIR", runtime.as_os_str()),
        ("XDG_STATE_HOME", state.as_os_str()),
        ("XDG_CONFIG_HOME", config.as_os_str()),
    ];

    let created = Command::new(binary)
        .envs(environment)
        .args([
            "new",
            "--detach",
            "--name",
            "e2e",
            "--",
            "/bin/sh",
            "-lc",
            "printf 'e2e-ready\\n'; sleep 10",
        ])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    thread::sleep(Duration::from_millis(250));

    let listed = Command::new(binary)
        .envs(environment)
        .args(["list", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let workspaces: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let pane_id = workspaces[0]["panes"][0]["id"].as_str().unwrap();
    let captured = Command::new(binary)
        .envs(environment)
        .args(["capture", "--pane", pane_id, "--lines", "20"])
        .output()
        .unwrap();
    assert!(captured.status.success());
    assert!(String::from_utf8_lossy(&captured.stdout).contains("e2e-ready"));

    let _ = Command::new(binary)
        .envs(environment)
        .args(["server", "stop"])
        .output();
}
