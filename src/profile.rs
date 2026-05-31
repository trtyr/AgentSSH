use crate::cli::{ConnectArgs, ProfileAction, ProfileCommand};
use crate::util::{DEFAULT_PORT, config_dir, emit_message, print_json};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct ProfileStore {
    pub profiles: BTreeMap<String, ConnectArgs>,
}

pub fn run_profile(command: ProfileCommand, json: bool) -> Result<()> {
    match command.command {
        ProfileAction::List => {
            let store = read_profiles()?;
            if json {
                print_json(&store.profiles)?;
                return Ok(());
            }
            if store.profiles.is_empty() {
                println!("No SSH profiles.");
                return Ok(());
            }
            for (name, profile) in store.profiles {
                println!(
                    "{name}\t{}@{}:{}",
                    profile.username.unwrap_or_default(),
                    profile.host.unwrap_or_default(),
                    profile.port.unwrap_or(DEFAULT_PORT)
                );
            }
            Ok(())
        }
        ProfileAction::Read { name } => {
            let store = read_profiles()?;
            let profile = store
                .profiles
                .get(&name)
                .ok_or_else(|| anyhow!("profile '{}' not found. Run 'agentssh profile list' to see saved profiles.", name))?;
            if json {
                print_json(profile)?;
                return Ok(());
            }
            println!("{}", serde_json::to_string_pretty(profile)?);
            Ok(())
        }
        ProfileAction::Add { name, connect } => {
            let mut store = read_profiles()?;
            let profile = if has_profile_add_flags(&connect) {
                connect
            } else {
                prompt_for_profile()?
            };
            store.profiles.insert(name.clone(), profile);
            write_profiles(&store)?;
            emit_message(json, "profile_written", &name)
        }
        ProfileAction::Write { name, data } => {
            let mut store = read_profiles()?;
            let profile: ConnectArgs =
                serde_json::from_str(&data).context("profile data must be a JSON object")?;
            store.profiles.insert(name.clone(), profile);
            write_profiles(&store)?;
            emit_message(json, "profile_written", &name)
        }
        ProfileAction::Delete { name } => {
            let mut store = read_profiles()?;
            store.profiles.remove(&name);
            write_profiles(&store)?;
            emit_message(json, "profile_deleted", &name)
        }
    }
}

pub fn read_profiles() -> Result<ProfileStore> {
    let path = profiles_path()?;
    if !path.exists() {
        return Ok(ProfileStore::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub fn write_profiles(store: &ProfileStore) -> Result<()> {
    let path = profiles_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(store)?))
        .with_context(|| format!("write {}", path.display()))
}

fn profiles_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profiles.json"))
}

fn has_profile_add_flags(connect: &ConnectArgs) -> bool {
    connect.host.is_some()
        || connect.port.is_some()
        || connect.username.is_some()
        || connect.password.is_some()
        || connect.private_key.is_some()
        || connect.passphrase.is_some()
        || connect.ready_timeout_ms.is_some()
        || connect.retry.is_some()
        || connect.retry_delay_ms.is_some()
}

fn prompt_for_profile() -> Result<ConnectArgs> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = io::stdout();

    write!(stdout, "Host: " )?;
    stdout.flush()?;
    let host = read_prompt_line(&mut reader)?;

    write!(stdout, "Username [root]: " )?;
    stdout.flush()?;
    let username = read_prompt_line(&mut reader)?;

    write!(stdout, "Port [22]: " )?;
    stdout.flush()?;
    let port = read_prompt_line(&mut reader)?;

    write!(stdout, "Private key (path, env var $ prefix, or inline content; leave empty to skip): " )?;
    stdout.flush()?;
    let private_key = read_prompt_line(&mut reader)?;

    build_profile_from_prompts(&format!("{}\n{}\n{}\n{}\n", host, username, port, private_key))
}

fn read_prompt_line(reader: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn build_profile_from_prompts(input: &str) -> Result<ConnectArgs> {
    let mut lines = input.lines();
    let host = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("host is required"))?
        .to_string();
    let username = lines.next().map(str::trim).unwrap_or("");
    let port_text = lines.next().map(str::trim).unwrap_or("");
    let private_key_text = lines.next().map(str::trim).unwrap_or("");

    let port = if port_text.is_empty() {
        DEFAULT_PORT
    } else {
        port_text.parse().context("port must be a valid u16")?
    };

    Ok(ConnectArgs {
        host: Some(host),
        username: Some(if username.is_empty() {
            "root".to_string()
        } else {
            username.to_string()
        }),
        port: Some(port),
        private_key: if private_key_text.is_empty() {
            None
        } else {
            Some(private_key_text.to_string())
        },
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_profile_from_interactive_input_uses_defaults() {
        let profile = build_profile_from_prompts("example.com\n\n\n\n").expect("profile should build");

        assert_eq!(profile.host.as_deref(), Some("example.com"));
        assert_eq!(profile.username.as_deref(), Some("root"));
        assert_eq!(profile.port, Some(22));
        assert_eq!(profile.private_key, None);
    }
}
