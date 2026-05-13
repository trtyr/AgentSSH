use crate::kernel;
use crate::profile;
use crate::protocol::WireRequest;
use crate::ssh_backend;
use crate::util::{DEFAULT_COLS, DEFAULT_LIMIT, DEFAULT_ROWS, DEFAULT_WAIT_MS};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "agentssh")]
#[command(about = "SSH toolkit for AI agents")]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    #[command(next_help_heading = "One-shot")]
    /// Run a single command and return stdout/stderr + exit code
    Exec(ExecCommand),
    /// Open an interactive PTY shell (for human use)
    Shell(ConnectArgs),

    #[command(next_help_heading = "Sessions")]
    /// Open a long-lived PTY session, returns session_id
    Connect(ConnectCommand),
    #[command(about = "Manage long-lived daemon-backed SSH sessions")]
    Session(SessionGroup),

    #[command(next_help_heading = "File transfer")]
    #[command(about = "Transfer files or list directories over SFTP")]
    File(FileGroup),

    #[command(next_help_heading = "Proxy")]
    #[command(about = "Manage daemon-backed SSH proxy listeners")]
    Proxy(ProxyGroup),

    #[command(next_help_heading = "Configuration")]
    /// Manage SSH connection profiles
    Profile(ProfileCommand),

    #[command(next_help_heading = "Daemon")]
    #[command(about = "Manage the background daemon lifecycle")]
    Daemon(DaemonGroup),
}

#[derive(Args, Debug)]
pub struct SessionGroup {
    #[command(subcommand)]
    pub command: SessionCommand,
}

#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// Send input to a session
    Send(SessionInputCommand),
    /// Spawn a new PTY session on an existing SSH connection
    Spawn(SpawnCommand),
    /// Read output from a session's cursor position
    Read(ReadCommand),
    /// Run a command on the session's SSH connection (clean stdout/stderr/exit code — no PTY echo)
    Exec(SessionExecCommand),
    /// Change PTY dimensions of a session
    Resize(ResizeCommand),
    /// Send a signal to a session (INT, TERM, KILL, etc.)
    Signal(SignalCommand),
    /// Get metadata for a session
    Status(StatusCommand),
    /// Check whether a session connection is still alive
    Ping(StatusCommand),
    /// List all active sessions
    List,
    /// Close a session
    Close(StatusCommand),
}

#[derive(Args, Debug)]
pub struct FileGroup {
    #[command(subcommand)]
    pub command: FileCommand,
}

#[derive(Subcommand, Debug)]
pub enum FileCommand {
    /// Upload a local file to the remote host
    Upload(TransferCommand),
    /// Download a remote file to the local host
    Download(TransferCommand),
    /// List files in a remote directory
    Ls(ListCommand),
    /// Write text content directly to a remote file
    Write(WriteCommand),
    /// Read a remote file to stdout
    Read(ReadFileCommand),
    /// Delete a remote file or directory
    Delete(DeleteFileCommand),
    /// Find-and-replace text in a remote file
    Edit(EditFileCommand),
}

#[derive(Args, Debug)]
pub struct DaemonGroup {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background daemon (auto-started by session commands if not running)
    Serve,
    /// Stop the daemon and close all sessions
    Shutdown,
}

#[derive(Args, Debug)]
pub struct ProxyGroup {
    #[command(subcommand)]
    pub command: ProxyCommand,
}

#[derive(Subcommand, Debug)]
pub enum ProxyCommand {
    /// Create a new daemon-managed SSH proxy
    Create(ProxyCreateCommand),
    /// List all active proxies
    List,
    /// Close one proxy or all proxies
    Close(ProxyCloseCommand),
    /// Check whether a proxy is still running
    Ping(ProxyPingCommand),
}

#[derive(Args, Debug)]
pub struct ProfileCommand {
    #[command(subcommand)]
    pub command: ProfileAction,
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    /// List all saved profiles
    List,
    /// Show the details of a specific profile
    Read {
        name: String,
    },
    #[command(about = "Add a profile quickly with flags or interactively via prompts")]
    Add {
        name: String,
        #[command(flatten)]
        connect: ConnectArgs,
    },
    /// Create or update a profile from a JSON string
    Write {
        name: String,
        #[arg(long)]
        data: String,
    },
    /// Delete a profile
    Delete {
        name: String,
    },
}

#[derive(Args, Clone, Serialize, Deserialize, Default, Debug)]
pub struct ConnectArgs {
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long)]
    pub password: Option<String>,
    #[arg(long)]
    pub password_env: Option<String>,
    #[arg(long)]
    pub private_key_path: Option<PathBuf>,
    #[arg(long)]
    pub private_key_env: Option<String>,
    #[arg(long)]
    pub passphrase: Option<String>,
    #[arg(long)]
    pub passphrase_env: Option<String>,
    #[arg(long)]
    pub ready_timeout_ms: Option<u64>,
    #[arg(long)]
    pub retry: Option<u32>,
    #[arg(long)]
    pub retry_delay_ms: Option<u64>,
}

#[derive(Args, Debug)]
pub struct ExecCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ConnectCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long, default_value_t = DEFAULT_COLS)]
    pub cols: u32,
    #[arg(long, default_value_t = DEFAULT_ROWS)]
    pub rows: u32,
    #[arg(long, default_value_t = DEFAULT_WAIT_MS)]
    pub wait_ms: u64,
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    /// Enable automatic reconnection when the SSH transport drops
    #[arg(long)]
    pub reconnect: bool,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct SessionInputCommand {
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub input: String,
    #[arg(long, default_value_t = false)]
    pub crlf: bool,
    #[arg(long, default_value_t = DEFAULT_WAIT_MS)]
    pub wait_ms: u64,
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    #[arg(long, conflicts_with = "strip_ansi")]
    pub raw: bool,
    #[arg(long, conflicts_with = "raw")]
    pub strip_ansi: bool,
    #[arg(long, default_value_t = false)]
    pub wait_for_exit: bool,
    #[arg(long)]
    pub timeout: Option<u64>,
    #[arg(long)]
    pub expect: Option<String>,
    #[arg(long)]
    pub respond: Option<String>,
    #[arg(long)]
    pub expect2: Option<String>,
    #[arg(long)]
    pub respond2: Option<String>,
    #[arg(long)]
    pub expect3: Option<String>,
    #[arg(long)]
    pub respond3: Option<String>,
}

impl SessionInputCommand {
    pub fn expect_pairs(&self) -> Result<Vec<ExpectRespondPair>> {
        crate::interactive::expect_pairs([
            (self.expect.clone(), self.respond.clone()),
            (self.expect2.clone(), self.respond2.clone()),
            (self.expect3.clone(), self.respond3.clone()),
        ])
    }
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct ExpectRespondPair {
    pub expect: String,
    pub respond: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct SpawnCommand {
    #[arg(long = "from")]
    pub from: String,
    #[arg(long, default_value_t = DEFAULT_COLS)]
    pub cols: u32,
    #[arg(long, default_value_t = DEFAULT_ROWS)]
    pub rows: u32,
    #[arg(long, default_value_t = DEFAULT_WAIT_MS)]
    pub wait_ms: u64,
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ReadCommand {
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub offset: Option<usize>,
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: usize,
    #[arg(long, default_value_t = DEFAULT_WAIT_MS)]
    pub wait_ms: u64,
    #[arg(long, default_value_t = false)]
    pub follow: bool,
    #[arg(long, conflicts_with = "strip_ansi")]
    pub raw: bool,
    #[arg(long, conflicts_with = "raw")]
    pub strip_ansi: bool,
    #[arg(long)]
    pub timeout: Option<u64>,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ResizeCommand {
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub cols: u32,
    #[arg(long)]
    pub rows: u32,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct SignalCommand {
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub signal: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct StatusCommand {
    #[arg(long)]
    pub session_id: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct SessionExecCommand {
    #[arg(long)]
    pub session_id: String,
    #[arg(required = true, trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct TransferCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub local: PathBuf,
    #[arg(long)]
    pub remote: PathBuf,
    #[arg(long, default_value = "auto", value_parser = ["auto", "sftp", "scp"])]
    pub method: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ListCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long, default_value = ".")]
    pub remote: PathBuf,
    #[arg(long, default_value = "auto", value_parser = ["auto", "sftp", "scp"])]
    pub method: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct WriteCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub remote: PathBuf,
    #[arg(long)]
    pub content: String,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ReadFileCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub remote: PathBuf,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct DeleteFileCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub remote: PathBuf,
    #[arg(long)]
    pub recursive: bool,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct EditFileCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(long)]
    pub remote: PathBuf,
    #[arg(long)]
    pub find: String,
    #[arg(long)]
    pub replace: String,
    #[arg(long)]
    pub regex: bool,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ProxyCreateCommand {
    #[command(flatten)]
    pub connect: ConnectArgs,
    /// Local listener address in host:port format
    #[arg(long, conflicts_with = "socks5")]
    pub local: Option<String>,
    /// Remote destination address in host:port format for local forwarding
    #[arg(long, requires = "local")]
    pub remote: Option<String>,
    /// Local SOCKS5 listener address in host:port format
    #[arg(long, conflicts_with = "local")]
    pub socks5: Option<String>,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ProxyCloseCommand {
    #[arg(long, conflicts_with = "all")]
    pub proxy_id: Option<String>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Clone, Serialize, Deserialize, Debug)]
pub struct ProxyPingCommand {
    #[arg(long)]
    pub proxy_id: String,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Exec(command) => ssh_backend::run_exec(command, cli.json),
        Command::Shell(connect) => ssh_backend::run_shell(connect, cli.json),
        Command::Connect(command) => kernel::run_client(WireRequest::Connect(command), cli.json),
        Command::Session(group) => match group.command {
            SessionCommand::Send(command) => kernel::run_client(WireRequest::Send(command), cli.json),
            SessionCommand::Exec(command) => kernel::run_client(WireRequest::Exec(command), cli.json),
            SessionCommand::Spawn(command) => {
                kernel::run_client(WireRequest::Spawn(command), cli.json)
            }
            SessionCommand::Read(command) => kernel::run_read_command(command, cli.json),
            SessionCommand::Resize(command) => {
                kernel::run_client(WireRequest::Resize(command), cli.json)
            }
            SessionCommand::Signal(command) => {
                kernel::run_client(WireRequest::Signal(command), cli.json)
            }
            SessionCommand::Status(command) => {
                kernel::run_client(WireRequest::Status(command), cli.json)
            }
            SessionCommand::Ping(command) => kernel::run_client(WireRequest::Ping(command), cli.json),
            SessionCommand::List => kernel::run_client(WireRequest::List, cli.json),
            SessionCommand::Close(command) => kernel::run_client(WireRequest::Close(command), cli.json),
        },
        Command::File(group) => match group.command {
            FileCommand::Upload(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::Upload(command), cli.json)
                } else {
                    ssh_backend::run_upload_once(command, cli.json)
                }
            }
            FileCommand::Download(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::Download(command), cli.json)
                } else {
                    ssh_backend::run_download_once(command, cli.json)
                }
            }
            FileCommand::Ls(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::Ls(command), cli.json)
                } else {
                    ssh_backend::run_ls_once(command, cli.json)
                }
            }
            FileCommand::Write(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::Write(command), cli.json)
                } else {
                    ssh_backend::run_write_once(command, cli.json)
                }
            }
            FileCommand::Read(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::ReadFile(command), cli.json)
                } else {
                    ssh_backend::run_read_once(command, cli.json)
                }
            }
            FileCommand::Delete(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::DeleteFile(command), cli.json)
                } else {
                    ssh_backend::run_delete_once(command, cli.json)
                }
            }
            FileCommand::Edit(command) => {
                if command.session_id.is_some() {
                    kernel::run_client(WireRequest::EditFile(command), cli.json)
                } else {
                    ssh_backend::run_edit_once(command, cli.json)
                }
            }
        },
        Command::Proxy(group) => match group.command {
            ProxyCommand::Create(command) => kernel::run_client(WireRequest::ProxyCreate(command), cli.json),
            ProxyCommand::List => kernel::run_client(WireRequest::ProxyList, cli.json),
            ProxyCommand::Close(command) => kernel::run_client(WireRequest::ProxyClose(command), cli.json),
            ProxyCommand::Ping(command) => kernel::run_client(WireRequest::ProxyPing(command), cli.json),
        },
        Command::Profile(command) => profile::run_profile(command, cli.json),
        Command::Daemon(group) => match group.command {
            DaemonCommand::Serve => kernel::run_server(),
            DaemonCommand::Shutdown => kernel::run_client(WireRequest::Shutdown, cli.json),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grouped_session_ping_command() {
        let cli = Cli::try_parse_from(["agentssh", "session", "ping", "--session-id", "s1"])
            .expect("session ping should parse");

        assert!(!cli.json);
        match cli.command {
            Command::Session(group) => match group.command {
                SessionCommand::Ping(command) => assert_eq!(command.session_id, "s1"),
                other => panic!("expected ping command, got {other:?}"),
            },
            other => panic!("expected session group, got {other:?}"),
        }
    }

    #[test]
    fn parses_follow_read_with_timeout() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "session",
            "read",
            "--session-id",
            "s1",
            "--follow",
            "--timeout",
            "1500",
            "--strip-ansi",
        ])
        .expect("follow read should parse");

        match cli.command {
            Command::Session(group) => match group.command {
                SessionCommand::Read(command) => {
                    assert!(command.follow);
                    assert_eq!(command.timeout, Some(1500));
                    assert!(!command.raw);
                    assert!(command.strip_ansi);
                }
                other => panic!("expected read command, got {other:?}"),
            },
            other => panic!("expected session group, got {other:?}"),
        }
    }

    #[test]
    fn rejects_conflicting_read_ansi_flags() {
        let error = Cli::try_parse_from([
            "agentssh",
            "session",
            "read",
            "--session-id",
            "s1",
            "--raw",
            "--strip-ansi",
        ])
        .expect_err("conflicting ansi flags should fail");

        assert!(error.to_string().contains("--raw"));
        assert!(error.to_string().contains("--strip-ansi"));
    }

    #[test]
    fn parses_session_spawn_command() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "session",
            "spawn",
            "--from",
            "s1",
        ])
        .expect("session spawn should parse");

        match cli.command {
            Command::Session(group) => match group.command {
                SessionCommand::Spawn(command) => assert_eq!(command.from, "s1"),
                other => panic!("expected spawn command, got {other:?}"),
            },
            other => panic!("expected session group, got {other:?}"),
        }
    }

    #[test]
    fn parses_session_send_expect_pairs() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "session",
            "send",
            "--session-id",
            "s1",
            "--input",
            "sudo apt update\n",
            "--expect",
            "[sudo] password",
            "--respond",
            "secret\n",
            "--expect2",
            "continue?",
            "--respond2",
            "y\n",
            "--wait-for-exit",
            "--timeout",
            "2500",
            "--raw",
        ])
        .expect("session send with expect pairs should parse");

        match cli.command {
            Command::Session(group) => match group.command {
                SessionCommand::Send(command) => {
                    let pairs = command.expect_pairs().expect("pairs should validate");
                    assert_eq!(pairs.len(), 2);
                    assert_eq!(pairs[0].expect, "[sudo] password");
                    assert_eq!(pairs[0].respond, "secret\n");
                    assert_eq!(pairs[1].expect, "continue?");
                    assert_eq!(pairs[1].respond, "y\n");
                    assert!(command.wait_for_exit);
                    assert_eq!(command.timeout, Some(2500));
                    assert!(command.raw);
                    assert!(!command.strip_ansi);
                }
                other => panic!("expected send command, got {other:?}"),
            },
            other => panic!("expected session group, got {other:?}"),
        }
    }

    #[test]
    fn rejects_conflicting_send_ansi_flags() {
        let error = Cli::try_parse_from([
            "agentssh",
            "session",
            "send",
            "--session-id",
            "s1",
            "--input",
            "echo hi\n",
            "--raw",
            "--strip-ansi",
        ])
        .expect_err("conflicting ansi flags should fail");

        assert!(error.to_string().contains("--raw"));
        assert!(error.to_string().contains("--strip-ansi"));
    }

    #[test]
    fn rejects_incomplete_expect_pair() {
        let command = SessionInputCommand {
            session_id: "s1".to_string(),
            input: "sudo whoami\n".to_string(),
            crlf: false,
            wait_ms: 100,
            limit: 1000,
            raw: false,
            strip_ansi: false,
            wait_for_exit: false,
            timeout: None,
            expect: Some("password".to_string()),
            respond: None,
            expect2: None,
            respond2: None,
            expect3: None,
            respond3: None,
        };

        let error = command.expect_pairs().expect_err("missing respond should fail");
        assert!(error.to_string().contains("--expect requires matching --respond"));
    }

    #[test]
    fn parses_grouped_file_upload_command() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "file",
            "upload",
            "--profile",
            "prod",
            "--local",
            "./local.txt",
            "--remote",
            "/tmp/remote.txt",
            "--method",
            "scp",
        ])
        .expect("file upload should parse");

        match cli.command {
            Command::File(group) => match group.command {
                FileCommand::Upload(command) => {
                    assert_eq!(command.connect.profile.as_deref(), Some("prod"));
                    assert_eq!(command.local, PathBuf::from("./local.txt"));
                    assert_eq!(command.remote, PathBuf::from("/tmp/remote.txt"));
                    assert_eq!(command.method, "scp");
                }
                other => panic!("expected upload command, got {other:?}"),
            },
            other => panic!("expected file group, got {other:?}"),
        }
    }

    #[test]
    fn file_ls_defaults_to_auto_method() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "file",
            "ls",
            "--profile",
            "prod",
            "--remote",
            "/tmp",
        ])
        .expect("file ls should parse");

        match cli.command {
            Command::File(group) => match group.command {
                FileCommand::Ls(command) => {
                    assert_eq!(command.connect.profile.as_deref(), Some("prod"));
                    assert_eq!(command.remote, PathBuf::from("/tmp"));
                    assert_eq!(command.method, "auto");
                }
                other => panic!("expected ls command, got {other:?}"),
            },
            other => panic!("expected file group, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_file_transfer_method() {
        let error = Cli::try_parse_from([
            "agentssh",
            "file",
            "download",
            "--profile",
            "prod",
            "--local",
            "./local.txt",
            "--remote",
            "/tmp/remote.txt",
            "--method",
            "ftp",
        ])
        .expect_err("invalid method should fail");

        assert!(error.to_string().contains("ftp"));
        assert!(error.to_string().contains("auto"));
    }

    #[test]
    fn parses_retry_flags_for_exec() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "exec",
            "--host",
            "example.com",
            "--retry",
            "3",
            "--retry-delay-ms",
            "250",
            "--",
            "uptime",
        ])
        .expect("exec retry flags should parse");

        match cli.command {
            Command::Exec(command) => {
                assert_eq!(command.connect.retry, Some(3));
                assert_eq!(command.connect.retry_delay_ms, Some(250));
            }
            other => panic!("expected exec command, got {other:?}"),
        }
    }

    #[test]
    fn parses_profile_add_command() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "profile",
            "add",
            "prod",
            "--host",
            "example.com",
            "--username",
            "root",
        ])
        .expect("profile add should parse");

        match cli.command {
            Command::Profile(ProfileCommand {
                command: ProfileAction::Add { name, connect },
            }) => {
                assert_eq!(name, "prod");
                assert_eq!(connect.host.as_deref(), Some("example.com"));
                assert_eq!(connect.username.as_deref(), Some("root"));
            }
            other => panic!("expected profile add command, got {other:?}"),
        }
    }

    #[test]
    fn parses_local_proxy_create_command() {
        let cli = Cli::try_parse_from([
            "agentssh",
            "proxy",
            "create",
            "--profile",
            "beijing",
            "--local",
            "127.0.0.1:9999",
            "--remote",
            "127.0.0.1:8080",
        ])
        .expect("proxy local create should parse");

        match cli.command {
            Command::Proxy(group) => match group.command {
                ProxyCommand::Create(command) => {
                    assert_eq!(command.connect.profile.as_deref(), Some("beijing"));
                    assert_eq!(command.local.as_deref(), Some("127.0.0.1:9999"));
                    assert_eq!(command.remote.as_deref(), Some("127.0.0.1:8080"));
                    assert_eq!(command.socks5, None);
                }
                other => panic!("expected proxy create command, got {other:?}"),
            },
            other => panic!("expected proxy group, got {other:?}"),
        }
    }

    #[test]
    fn rejects_proxy_create_with_both_local_and_socks5() {
        let error = Cli::try_parse_from([
            "agentssh",
            "proxy",
            "create",
            "--host",
            "example.com",
            "--local",
            "127.0.0.1:9999",
            "--remote",
            "127.0.0.1:8080",
            "--socks5",
            "127.0.0.1:1080",
        ])
        .expect_err("local and socks5 should conflict");

        assert!(error.to_string().contains("--local"));
        assert!(error.to_string().contains("--socks5"));
    }
}

