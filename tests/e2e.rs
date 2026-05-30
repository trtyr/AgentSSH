use std::process::Command;
use std::path::PathBuf;
use std::fs;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `agentssh` with the given args. Returns (stdout, stderr, exit_code).
fn agentssh(args: &[&str]) -> (String, String, i32) {
    let output = Command::new("cargo")
        .args(["run", "-q", "--"])
        .args(args)
        .output()
        .expect("failed to execute agentssh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

/// Same as `agentssh` but injects `AGENTSSH_CONFIG_DIR` for profile isolation.
fn agentssh_in_dir(dir: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let output = Command::new("cargo")
        .args(["run", "-q", "--"])
        .args(args)
        .env("AGENTSSH_CONFIG_DIR", dir)
        .output()
        .expect("failed to execute agentssh");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

/// Create a unique temp directory for test isolation.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agentssh-e2e-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Expected version from Cargo.toml.
fn expected_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ===================================================================
// P0: CLI Basics — no SSH, no daemon
// ===================================================================

#[test]
fn cli_help_exits_zero_and_shows_usage() {
    let (stdout, stderr, code) = agentssh(&["--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(combined.contains("agentssh"), "output should mention agentssh");
    assert!(combined.contains("profile"), "output should list profile subcommand");
    assert!(combined.contains("exec"), "output should list exec subcommand");
}

#[test]
fn cli_version_matches_cargo_toml() {
    let (stdout, stderr, code) = agentssh(&["--version"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let version = expected_version();
    assert!(
        stdout.contains(&version),
        "stdout '{}' should contain version '{}'",
        stdout.trim(),
        version
    );
}

#[test]
fn cli_invalid_subcommand_fails() {
    let (_stdout, stderr, code) = agentssh(&["nonexistent-command"]);
    assert_ne!(code, 0, "invalid subcommand should exit non-zero");
    assert!(
        stderr.contains("error") || stderr.contains("unrecognized"),
        "stderr should contain error message: {stderr}"
    );
}

#[test]
fn cli_help_per_subcommand() {
    for subcmd in &["profile", "exec", "connect", "session", "file", "proxy", "daemon"] {
        let (stdout, stderr, code) = agentssh(&[subcmd, "--help"]);
        assert_eq!(code, 0, "{subcmd} --help failed: stderr={stderr}");
        assert!(!stdout.trim().is_empty(), "{subcmd} --help produced no output");
    }
}

// ===================================================================
// P0: Profile CRUD — isolated via AGENTSSH_CONFIG_DIR
// ===================================================================

#[test]
fn profile_list_empty_when_no_profiles() {
    let dir = temp_dir("profile-empty");
    let (stdout, _stderr, code) = agentssh_in_dir(&dir, &["profile", "list"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("No SSH profiles") || stdout.trim().is_empty(),
        "expected empty message, got: {stdout}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_write_then_list_shows_profile() {
    let dir = temp_dir("profile-list");
    // Write a profile
    let (stdout, stderr, code) = agentssh_in_dir(
        &dir,
        &[
            "profile", "write", "test-host",
            "--data", r#"{"host":"10.0.0.1","username":"admin","port":2222}"#,
        ],
    );
    assert_eq!(code, 0, "write failed: stderr={stderr} stdout={stdout}");
    assert!(stdout.contains("profile_written") || stdout.contains("test-host"));

    // List should show it
    let (stdout, _stderr, code) = agentssh_in_dir(&dir, &["profile", "list"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("test-host"), "list should contain test-host: {stdout}");
    assert!(stdout.contains("10.0.0.1"), "list should contain host: {stdout}");
    assert!(stdout.contains("admin"), "list should contain username: {stdout}");
    assert!(stdout.contains("2222"), "list should contain port: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_write_then_read_shows_json() {
    let dir = temp_dir("profile-read");
    agentssh_in_dir(
        &dir,
        &[
            "profile", "write", "myserver",
            "--data", r#"{"host":"example.com","username":"root"}"#,
        ],
    );

    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["profile", "read", "myserver"]);
    assert_eq!(code, 0, "read failed: stderr={stderr}");
    assert!(stdout.contains("example.com"), "should show host: {stdout}");
    assert!(stdout.contains("root"), "should show username: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_read_nonexistent_fails() {
    let dir = temp_dir("profile-read-miss");
    let (_stdout, stderr, code) = agentssh_in_dir(&dir, &["profile", "read", "ghost"]);
    assert_ne!(code, 0, "reading nonexistent profile should fail");
    assert!(
        stderr.contains("not found") || stderr.contains("ghost"),
        "error should mention missing profile: {stderr}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_delete_removes_profile() {
    let dir = temp_dir("profile-delete");
    // Create
    agentssh_in_dir(
        &dir,
        &["profile", "write", "doomed", "--data", r#"{"host":"1.2.3.4"}"#],
    );

    // Delete
    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["profile", "delete", "doomed"]);
    assert_eq!(code, 0, "delete failed: stderr={stderr}");
    assert!(stdout.contains("profile_deleted") || stdout.contains("doomed"));

    // List should be empty
    let (stdout, _stderr, code) = agentssh_in_dir(&dir, &["profile", "list"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("No SSH profiles") || stdout.trim().is_empty(),
        "profile should be gone: {stdout}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_write_overwrites_existing() {
    let dir = temp_dir("profile-overwrite");
    // Write v1
    agentssh_in_dir(
        &dir,
        &["profile", "write", "srv", "--data", r#"{"host":"old.example.com"}"#],
    );
    // Write v2
    agentssh_in_dir(
        &dir,
        &["profile", "write", "srv", "--data", r#"{"host":"new.example.com"}"#],
    );

    let (stdout, _stderr, code) = agentssh_in_dir(&dir, &["profile", "read", "srv"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("new.example.com"), "should have new host: {stdout}");
    assert!(!stdout.contains("old.example.com"), "should not have old host: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_write_invalid_json_fails() {
    let dir = temp_dir("profile-bad-json");
    let (_stdout, stderr, code) = agentssh_in_dir(
        &dir,
        &["profile", "write", "bad", "--data", "not-json"],
    );
    assert_ne!(code, 0, "invalid JSON should fail");
    assert!(
        stderr.contains("JSON") || stderr.contains("json") || stderr.contains("parse"),
        "error should mention JSON: {stderr}"
    );
    let _ = fs::remove_dir_all(&dir);
}

// ===================================================================
// P0: JSON output mode
// ===================================================================

#[test]
fn profile_list_json_mode_returns_valid_json() {
    let dir = temp_dir("profile-json-list");
    agentssh_in_dir(
        &dir,
        &["profile", "write", "jhost", "--data", r#"{"host":"json.example.com"}"#],
    );

    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["--json", "profile", "list"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(&stdout.trim());
    assert!(parsed.is_ok(), "output should be valid JSON: {stdout}");
    let val = parsed.unwrap();
    assert!(val.get("ok").is_some() || val.is_object(), "should be JSON object: {stdout}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_write_json_mode_returns_json_event() {
    let dir = temp_dir("profile-json-write");
    let (stdout, stderr, code) = agentssh_in_dir(
        &dir,
        &[
            "--json", "profile", "write", "js",
            "--data", r#"{"host":"js.example.com"}"#,
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["ok"], true);
    assert_eq!(val["data"]["event"], "profile_written");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn profile_delete_json_mode_returns_json_event() {
    let dir = temp_dir("profile-json-delete");
    agentssh_in_dir(
        &dir,
        &["profile", "write", "delme", "--data", r#"{"host":"x"}"#],
    );

    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["--json", "profile", "delete", "delme"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["ok"], true);
    assert_eq!(val["data"]["event"], "profile_deleted");
    let _ = fs::remove_dir_all(&dir);
}

// ===================================================================
// P0: Error output in JSON mode
// ===================================================================

#[test]
fn json_mode_errors_are_json_formatted() {
    let dir = temp_dir("json-error");
    let (stdout, _stderr, code) = agentssh_in_dir(
        &dir,
        &["--json", "profile", "read", "nonexistent"],
    );
    assert_ne!(code, 0);
    // In --json mode, errors should go to stdout as JSON
    if !stdout.trim().is_empty() {
        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(stdout.trim());
        assert!(parsed.is_ok(), "error output should be valid JSON in --json mode: {stdout}");
        let val = parsed.unwrap();
        assert_eq!(val["ok"], false, "error JSON should have ok: false");
    }
    let _ = fs::remove_dir_all(&dir);
}

// ===================================================================
// P1: Daemon status (no SSH needed)
// ===================================================================

#[test]
fn daemon_status_exits_zero() {
    let (stdout, _stderr, code) = agentssh(&["daemon", "status"]);
    // Should succeed regardless of whether daemon is running
    assert_eq!(code, 0);
    // Either "not running" or actual status JSON
    assert!(
        stdout.contains("not running") || stdout.contains("sessions") || stdout.contains("Daemon"),
        "should show daemon status: {stdout}"
    );
}

#[test]
fn daemon_status_json_exits_zero() {
    let (stdout, _stderr, code) = agentssh(&["--json", "daemon", "status"]);
    assert_eq!(code, 0);

    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    // Daemon might be running (wrapped in {"ok":true,"data":{...}}) or not ({"running":false})
    let has_data = val.get("data").is_some() || val.get("running").is_some();
    assert!(has_data, "should be valid daemon status JSON: {stdout}");
}

// ===================================================================
// P1: CLI flag validation
// ===================================================================

#[test]
fn conflicting_json_and_output_flags() {
    let (_stdout, stderr, code) = agentssh(&["--json", "--output", "text", "profile", "list"]);
    assert_ne!(code, 0, "conflicting flags should fail");
    assert!(
        stderr.contains("conflict") || stderr.contains("cannot"),
        "should mention conflict: {stderr}"
    );
}

#[test]
fn session_read_conflicting_raw_and_strip_ansi() {
    let (_stdout, stderr, code) = agentssh(&[
        "session", "read",
        "--session-id", "s1",
        "--raw", "--strip-ansi",
    ]);
    assert_ne!(code, 0, "conflicting flags should fail");
    assert!(
        stderr.contains("conflict") || stderr.contains("cannot"),
        "should mention conflict: {stderr}"
    );
}

#[test]
fn proxy_create_conflicting_local_and_socks5() {
    let (_stdout, stderr, code) = agentssh(&[
        "proxy", "create",
        "-H", "localhost",
        "--local", "127.0.0.1:8080",
        "--remote", "127.0.0.1:9090",
        "--socks5", "127.0.0.1:1080",
    ]);
    assert_ne!(code, 0, "conflicting proxy flags should fail");
    assert!(
        stderr.contains("conflict") || stderr.contains("cannot"),
        "should mention conflict: {stderr}"
    );
}

#[test]
fn session_send_missing_expect_respond_pair() {
    // --expect without --respond: validation happens at session handler level,
    // so we may get "not found" (session doesn't exist) or "respond" error.
    let (_stdout, stderr, code) = agentssh(&[
        "session", "send",
        "--session-id", "s1",
        "--input", "ls",
        "--expect", "password",
    ]);
    assert_ne!(code, 0, "should fail");
    // Accept either error: session not found, or missing respond validation
    assert!(
        stderr.contains("respond") || stderr.contains("not found") || stderr.contains("error"),
        "should show error: {stderr}"
    );
}

// ===================================================================
// P1: Profile add with flags (non-interactive)
// ===================================================================

#[test]
fn profile_add_with_flags_creates_profile() {
    let dir = temp_dir("profile-add-flags");
    let (stdout, stderr, code) = agentssh_in_dir(
        &dir,
        &[
            "profile", "add", "flagged",
            "-H", "flag.example.com",
            "-u", "testuser",
            "-P", "2222",
        ],
    );
    assert_eq!(code, 0, "profile add failed: stderr={stderr}");
    assert!(stdout.contains("profile_written") || stdout.contains("flagged"));

    // Verify by reading
    let (stdout, _stderr, code) = agentssh_in_dir(&dir, &["profile", "read", "flagged"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("flag.example.com"));
    assert!(stdout.contains("testuser"));
    assert!(stdout.contains("2222"));
    let _ = fs::remove_dir_all(&dir);
}

// ===================================================================
// P1: File transfer method validation
// ===================================================================

#[test]
fn file_upload_invalid_method_fails() {
    let (_stdout, stderr, code) = agentssh(&[
        "file", "upload",
        "-H", "localhost",
        "--local", "/tmp/test",
        "--remote", "/tmp/test",
        "--method", "ftp",
    ]);
    assert_ne!(code, 0, "invalid method should fail");
    assert!(
        stderr.contains("ftp") || stderr.contains("invalid") || stderr.contains("possible values"),
        "should mention invalid method: {stderr}"
    );
}

// ===================================================================
// P1: Exec requires command
// ===================================================================

#[test]
fn exec_without_command_fails() {
    let (_stdout, stderr, code) = agentssh(&["exec", "-H", "localhost"]);
    assert_ne!(code, 0, "exec without command should fail");
    assert!(
        stderr.contains("required") || stderr.contains("command"),
        "should mention missing command: {stderr}"
    );
}

// ===================================================================
// P2: SSH-dependent tests (require a running SSH server)
// Set AGENTSSH_E2E_SSH_HOST / AGENTSSH_E2E_SSH_USER to enable.
// ===================================================================

fn ssh_test_env() -> Option<(String, String, u16)> {
    let host = std::env::var("AGENTSSH_E2E_SSH_HOST").ok()?;
    let user = std::env::var("AGENTSSH_E2E_SSH_USER").unwrap_or_else(|_| "root".to_string());
    let port: u16 = std::env::var("AGENTSSH_E2E_SSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(22);
    Some((host, user, port))
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_exec_echo
fn ssh_exec_echo() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };
    let (stdout, stderr, code) = agentssh(&[
        "exec",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--", "echo hello-e2e",
    ]);
    assert_eq!(code, 0, "exec failed: stderr={stderr}");
    assert!(stdout.contains("hello-e2e"), "stdout should contain echo output: {stdout}");
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_exec_exit_code
fn ssh_exec_exit_code() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };
    let (stdout, stderr, code) = agentssh(&[
        "--json", "exec",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--", "exit 42",
    ]);
    assert_eq!(code, 0, "agentssh itself should succeed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["data"]["exit_code"], 42, "remote exit code should be 42");
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_exec_shell_pipe
fn ssh_exec_shell_pipe() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };
    let (stdout, stderr, code) = agentssh(&[
        "exec",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--", "echo hello | tr a-z A-Z",
    ]);
    assert_eq!(code, 0, "exec with pipe failed: stderr={stderr}");
    assert!(stdout.contains("HELLO"), "pipe should uppercase: {stdout}");
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_exec_stderr_capture
fn ssh_exec_stderr_capture() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };
    let (stdout, stderr, code) = agentssh(&[
        "--json", "exec",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--", "echo err-msg >&2",
    ]);
    assert_eq!(code, 0, "agentssh should succeed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert!(
        val["data"]["stderr"].as_str().unwrap_or("").contains("err-msg"),
        "stderr should contain err-msg"
    );
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_exec_json_output
fn ssh_exec_json_output() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };
    let (stdout, stderr, code) = agentssh(&[
        "--json", "exec",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--", "echo hi",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["ok"], true);
    assert!(val["data"]["stdout"].as_str().unwrap_or("").contains("hi"));
    assert!(val["data"].get("exit_code").is_some(), "should have exit_code field");
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_connect_session_lifecycle
fn ssh_connect_session_lifecycle() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };

    let dir = temp_dir("session-life");
    // Create profile
    agentssh_in_dir(&dir, &[
        "profile", "write", "e2e",
        "--data", &format!(r#"{{"host":"{}","username":"{}","port":{}}}"#, host, user, port),
    ]);

    // Connect
    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["--json", "connect", "-p", "e2e"]);
    assert_eq!(code, 0, "connect failed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let session_id = val["data"]["session_id"].as_str().expect("session_id").to_string();

    // Ping
    let (stdout, stderr, code) = agentssh_in_dir(&dir, &[
        "--json", "session", "ping", "--session-id", &session_id,
    ]);
    assert_eq!(code, 0, "ping failed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["data"]["status"], "running");

    // List
    let (stdout, stderr, code) = agentssh_in_dir(&dir, &["--json", "session", "list"]);
    assert_eq!(code, 0, "list failed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert!(val["data"].as_array().unwrap().len() >= 1);

    // Close
    let (stdout, stderr, code) = agentssh_in_dir(&dir, &[
        "--json", "session", "close", "--session-id", &session_id,
    ]);
    assert_eq!(code, 0, "close failed: stderr={stderr}");
    let val: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(val["ok"], true);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_file_upload_download
fn ssh_file_upload_download() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };

    let dir = temp_dir("file-transfer");
    let local_upload = dir.join("upload.txt");
    let local_download = dir.join("download.txt");
    fs::write(&local_upload, "e2e-test-content-12345").expect("write upload file");

    // Upload
    let (_stdout, stderr, code) = agentssh(&[
        "file", "upload",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--local", local_upload.to_str().unwrap(),
        "--remote", "/tmp/agentssh-e2e-upload.txt",
    ]);
    assert_eq!(code, 0, "upload failed: stderr={stderr}");

    // Download
    let (_stdout, stderr, code) = agentssh(&[
        "file", "download",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--local", local_download.to_str().unwrap(),
        "--remote", "/tmp/agentssh-e2e-upload.txt",
    ]);
    assert_eq!(code, 0, "download failed: stderr={stderr}");

    // Verify content matches
    let downloaded = fs::read_to_string(&local_download).expect("read downloaded file");
    assert_eq!(downloaded, "e2e-test-content-12345");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_file_write_read_delete
fn ssh_file_write_read_delete() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };

    let remote_path = "/tmp/agentssh-e2e-writeread.txt";
    let port_str = port.to_string();

    // Write
    let (_stdout, stderr, code) = agentssh(&[
        "file", "write",
        "-H", &host, "-u", &user, "-P", &port_str,
        "--remote", remote_path, "--content", "hello-from-e2e",
    ]);
    assert_eq!(code, 0, "write failed: stderr={stderr}");

    // Read
    let (stdout, stderr, code) = agentssh(&[
        "file", "read",
        "-H", &host, "-u", &user, "-P", &port_str,
        "--remote", remote_path,
    ]);
    assert_eq!(code, 0, "read failed: stderr={stderr}");
    assert!(stdout.contains("hello-from-e2e"), "content mismatch: {stdout}");

    // Delete
    let (_stdout, stderr, code) = agentssh(&[
        "file", "delete",
        "-H", &host, "-u", &user, "-P", &port_str,
        "--remote", remote_path,
    ]);
    assert_eq!(code, 0, "delete failed: stderr={stderr}");
}

#[test]
#[ignore] // run with: cargo test --test e2e -- --ignored ssh_file_ls
fn ssh_file_ls() {
    let Some((host, user, port)) = ssh_test_env() else {
        eprintln!("skipping: set AGENTSSH_E2E_SSH_HOST");
        return;
    };

    let (stdout, stderr, code) = agentssh(&[
        "file", "ls",
        "-H", &host,
        "-u", &user,
        "-P", &port.to_string(),
        "--remote", "/tmp",
    ]);
    assert_eq!(code, 0, "ls failed: stderr={stderr}");
    assert!(!stdout.trim().is_empty(), "ls /tmp should produce output");
}
