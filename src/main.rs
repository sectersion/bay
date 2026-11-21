#![allow(dead_code)] // skeleton: engine seam is design, not yet wired (M1)

use clap::{Parser, Subcommand};

mod bake;
mod ch50;
mod client;
mod daemon;
mod engine;
mod protocol;

use protocol::Request;

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
    /// Ask the daemon if it is alive, print its reply
    Ping,
    /// Run the fleet daemon (normally spawned by the CLI)
    #[command(hide = true)]
    Daemon,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Daemon => daemon::run(),
        // bake is the one command not yet routed through bayd: dispatch is
        // stateless and the daemon holds no engine yet (#15). Flip this to
        // `submit(Request::Bake { .. })` when bake moves daemon-internal.
        Cmd::Bake { manifest } => {
            let out = bake::run(&manifest)?;
            println!("{}", serde_json::to_string(&out)?);
            Ok(())
        }
        Cmd::Ping => submit(Request::Ping),
        Cmd::Up { n, repo } => submit(Request::Up { n, repo }),
        Cmd::Exec { bay, cmd } => submit(Request::Exec { bay, argv: cmd }),
        Cmd::Halt { bay } => submit(Request::Halt { bay }),
        Cmd::Wake { bay } => submit(Request::Wake { bay }),
        Cmd::Kill { bay, all } => submit(Request::Kill { bay, all }),
    }
}

/// Thin client: send one request, print one reply line, exit nonzero on error.
fn submit(req: Request) -> anyhow::Result<()> {
    let resp = client::request(&req)?;
    println!("{}", serde_json::to_string(&resp)?);
    if matches!(resp, protocol::Response::Err { .. }) {
        std::process::exit(1);
    }
    Ok(())
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
