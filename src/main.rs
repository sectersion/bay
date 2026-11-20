#![allow(dead_code)] // skeleton: engine seam is design, not yet wired (M1)

use clap::{Parser, Subcommand};

mod engine;

#[derive(Parser)]
#[command(name = "bay", version, about = "give each agent its own bay")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bake the golden snapshot from a manifest
    Bake { manifest: std::path::PathBuf },
    /// Bring up N bays from the golden snapshot
    Up {
        n: u32,
        /// local repo to clone into each bay (file:// clone, no remote)
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
    /// Run a command inside a bay, terminal pass-through
    Exec {
        bay: u32,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Park a bay: snapshot it, free its RAM
    Halt { bay: u32 },
    /// Wake a parked bay in place
    Wake { bay: u32 },
    /// Destroy a bay, or all of them
    Kill {
        bay: Option<u32>,
        #[arg(long)]
        all: bool,
    },
    /// Run the fleet daemon (normally spawned by the CLI)
    #[command(hide = true)]
    Daemon,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _ = cli.cmd;
    Err(anyhow::anyhow!(
        "not implemented — see ROADMAP.md (M0 spike first)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_owns_its_args() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["bay", "exec", "2", "--", "claude", "hi"]).unwrap();
        matches!(cli.cmd, Cmd::Exec { bay: 2, .. });
    }
}
