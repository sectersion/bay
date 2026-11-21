//! bayd: the fleet daemon (#14). Owns $BAY_HOME (default ~/.bay) and the
//! unix socket the CLI talks to. Only thing allowed to touch fleet state.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use crate::protocol::{dispatch, Request, Response};

pub const WINDOWS_MSG: &str = "bayd is not yet available on Windows (use WSL2)";

/// Where bayd lives. `BAY_HOME` overrides the default `~/.bay`.
pub fn bay_home() -> anyhow::Result<PathBuf> {
    if let Some(h) = std::env::var_os("BAY_HOME") {
        return Ok(h.into());
    }
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(PathBuf::from)
        .map(|h| h.join(".bay"))
        .ok_or_else(|| anyhow::anyhow!("no home directory found; set BAY_HOME"))
}

pub fn sock_path() -> anyhow::Result<PathBuf> {
    Ok(bay_home()?.join("bayd.sock"))
}

#[cfg(unix)]
pub fn run() -> anyhow::Result<()> {
    let home = bay_home()?;
    std::fs::create_dir_all(&home)?; // golden/ and bays/ are created lazily by bake/up
    let sock = home.join("bayd.sock");
    // ponytail: socket perms are host-umask default; chmod 0600 it when
    // multi-user hosts become a threat model (needs nix/libc, deferred).
    let listener = bind(&sock)?;
    eprintln!("bayd: listening on {}", sock.display());
    serve(listener)
}

#[cfg(not(unix))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("{WINDOWS_MSG}")
}

#[cfg(unix)]
fn bind(sock: &Path) -> anyhow::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::net::{UnixListener, UnixStream};
    if sock.exists() {
        if UnixStream::connect(sock).is_ok() {
            anyhow::bail!("bayd already running: {}", sock.display());
        }
        std::fs::remove_file(sock)?; // stale socket from a dead bayd
    }
    Ok(UnixListener::bind(sock)?)
}

#[cfg(unix)]
fn serve(listener: std::os::unix::net::UnixListener) -> anyhow::Result<()> {
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                std::thread::spawn(move || {
                    if let Err(e) = handle(conn) {
                        eprintln!("bayd: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("bayd: accept failed: {e}"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn handle(conn: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
    let mut conn = BufReader::new(conn);
    let mut line = String::new();
    loop {
        line.clear();
        if conn.read_line(&mut line)? == 0 {
            return Ok(()); // client hung up
        }
        let req: Request = match serde_json::from_str(line.trim_end()) {
            Ok(req) => req,
            Err(_) => {
                reply(&mut conn, &Response::Err { error: "BAD_JSON".into() })?;
                return Ok(());
            }
        };
        reply(&mut conn, &dispatch(&req))?;
    }
}

#[cfg(unix)]
fn reply(conn: &mut BufReader<std::os::unix::net::UnixStream>, resp: &Response) -> anyhow::Result<()> {
    conn.get_mut().write_all(serde_json::to_string(resp)?.as_bytes())?;
    conn.get_mut().write_all(b"\n")?;
    Ok(())
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn socket_ping_round_trip() {
        // short name: AF_UNIX path budget is ~108 bytes, macOS temp dirs are long
        let sock = std::env::temp_dir().join(format!("bayd-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener: UnixListener = bind(&sock).unwrap();
        std::thread::spawn(move || {
            let _ = serve(listener);
        });
        let resp = crate::client::round_trip(&sock, &Request::Ping).unwrap();
        assert_eq!(resp, Response::Ok);
        let _ = std::fs::remove_file(&sock);
    }
}
