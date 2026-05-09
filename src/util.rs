use anyhow::{Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use ssh2::FileStat;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_PORT: u16 = 22;
pub const DEFAULT_READY_TIMEOUT_MS: u64 = 20_000;
pub const DEFAULT_COLS: u32 = 120;
pub const DEFAULT_ROWS: u32 = 40;
pub const DEFAULT_LIMIT: usize = 8_000;
pub const DEFAULT_WAIT_MS: u64 = 250;
pub const MAX_BUFFER: usize = 1_000_000;

#[derive(Serialize)]
pub struct JsonOutput<T: Serialize> {
    ok: bool,
    data: T,
}

pub fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&JsonOutput {
            ok: true,
            data: value
        })?
    );
    Ok(())
}

pub fn emit_message(json: bool, event: &str, message: &str) -> Result<()> {
    if json {
        print_json(&serde_json::json!({ "event": event, "message": message }))?;
        return Ok(());
    }
    println!("{event}: {message}");
    Ok(())
}

pub fn sleep_ms(ms: u64) {
    if ms > 0 {
        thread::sleep(Duration::from_millis(ms));
    }
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn stat_json(stat: FileStat) -> Value {
    serde_json::json!({
        "size": stat.size,
        "uid": stat.uid,
        "gid": stat.gid,
        "perm": stat.perm,
        "atime": stat.atime,
        "mtime": stat.mtime,
    })
}

/// Strip ANSI escape sequences from a string.
/// Removes CSI (\x1b[...m/K/etc), OSC (\x1b]...\x07/\x1b\\), and simple ESC sequences.
pub fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI: ESC [ ... (0x40-0x7E)
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1; // skip the final byte
                    }
                }
                b']' => {
                    // OSC: ESC ] ... (BEL or ESC\)
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 || (bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\') {
                            if bytes[i] == 0x1b {
                                i += 2;
                            } else {
                                i += 1;
                            }
                            break;
                        }
                        i += 1;
                    }
                }
                _ => {
                    // Other ESC sequences: skip ESC + next char
                    i += 2;
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AGENTSSH_CONFIG_DIR") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| anyhow!("could not locate config directory. Set AGENTSSH_CONFIG_DIR or ensure your home directory is accessible."))?;
    Ok(root.join("agentssh"))
}

pub fn runtime_socket_path() -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("agentssh-{user}.sock"))
}

pub fn log_daemon(message: &str) -> Result<()> {
    let path = std::env::var("AGENTSSH_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/agentssh-daemon.log"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "[{}] {}", now_ms(), message)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_daemon_writes_timestamped_line_to_env_path() {
        let path = std::env::temp_dir().join(format!("agentssh-daemon-test-{}.log", now_ms()));
        unsafe { std::env::set_var("AGENTSSH_LOG", &path); }
        if path.exists() {
            std::fs::remove_file(&path).expect("remove old log file");
        }

        log_daemon("daemon started").expect("log write should succeed");

        let text = std::fs::read_to_string(&path).expect("read log file");
        assert!(text.contains("daemon started"));
        assert!(text.starts_with('['));

        let _ = std::fs::remove_file(&path);
        unsafe { std::env::remove_var("AGENTSSH_LOG"); }
    }
}
