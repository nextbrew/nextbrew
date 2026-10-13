use clap::{CommandFactory, Parser};
use clap_complete::generate;
use std::io;

#[derive(Parser)]
#[command(name = "nb")]
#[command(about = "NextBrew - A fast Homebrew-compatible package installer")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    command: crate::cli::Commands,
}

pub fn execute(shell: clap_complete::shells::Shell) -> Result<(), nb_core::Error> {
    let mut cmd = crate::cli::Cli::command();
    generate(shell, &mut cmd, "nb", &mut io::stdout());
    Ok(())
}
