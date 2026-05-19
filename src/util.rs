use anyhow::{Result, anyhow};
use clap::ValueEnum;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Structured JSON output (same as --json)
    Json,
    /// Plain text output — stdout to stdout, stderr to stderr
    Text,
}

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

/// Strip ANSI escape sequences, Private Use Area characters (Nerd Font icons),
/// and non-printable control characters from PTY output.
///
/// Keeps: printable characters, newlines (`\n`), carriage returns (`\r`), tabs (`\t`).
/// Removes: ANSI escape sequences (CSI/OSC/simple ESC), Private Use Area
///          codepoints (`U+E000`–`U+F8FF`), and other control characters.
pub fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            let next = chars.next();
            match next {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7E').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.next() == Some('\\') {
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if is_keepable(ch) {
            result.push(ch);
        }
    }
    result
}

fn is_keepable(ch: char) -> bool {
    match ch {
        '\n' | '\r' | '\t' => true,
        _ if ch.is_control() => false,
        _ if ('\u{E000}'..='\u{F8FF}').contains(&ch) => false,
        _ => true,
    }
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

    #[test]
    fn strip_ansi_removes_csi_color_codes() {
        let input = "hello \x1b[31mred\x1b[0m world";
        assert_eq!(strip_ansi(input), "hello red world");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        let input = "title\x1b]0;window title\x07body";
        assert_eq!(strip_ansi(input), "titlebody");
    }

    #[test]
    fn strip_ansi_removes_private_use_area_characters() {
        let folder = '\u{F31B}';
        let home = '\u{F015}';
        let input = format!("{folder} {home} ~");
        assert_eq!(strip_ansi(&input), "  ~");
    }

    #[test]
    fn strip_ansi_removes_control_characters_except_whitespace() {
        assert_eq!(strip_ansi("a\x07b\x08c"), "abc");
        assert_eq!(strip_ansi("line1\nline2\r\nline3"), "line1\nline2\r\nline3");
        assert_eq!(strip_ansi("col1\tcol2"), "col1\tcol2");
    }

    #[test]
    fn strip_ansi_handles_empty_and_clean_input() {
        assert_eq!(strip_ansi(""), "");
        assert_eq!(strip_ansi("clean text"), "clean text");
    }

    #[test]
    fn strip_ansi_handles_nested_ansi_and_pua() {
        let folder = '\u{F31B}';
        let input = format!("\x1b[32m{folder}\x1b[0m README.md");
        assert_eq!(strip_ansi(&input), " README.md");
    }
}
