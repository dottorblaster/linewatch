use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "linewatch", about = "Linewatch monitoring daemon")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the monitoring daemon
    Run,
    /// Report on collected data
    Report,
}
