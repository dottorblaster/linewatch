mod cli;
mod config;
mod core;
mod shell;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let _config = Config::load()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Run => {
            shell::run::run(_config).await?;
        }
        Command::Report => {
            println!("report: not implemented");
        }
    }

    Ok(())
}
