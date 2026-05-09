mod cli;
mod connection;
mod interactive;
mod kernel;
mod profile;
mod protocol;
mod proxy;
mod session;
mod sftp;
mod ssh_backend;
mod util;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run()
}
