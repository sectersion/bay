//! bayd: the fleet daemon (#14). Owns $BAY_HOME (default ~/.bay) and the
//! unix socket the CLI talks to. Only thing allowed to touch fleet state.

#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;
use std::path::Path;
#[cfg(unix)]
use crate::protocol::{dispatch, Request, Response};

/// One engine for the whole daemon lifetime (FLEET/ARCHITECTURE: only the
/// daemon may own VMs). `up` boots through it; `exec` only dials the bay's
/// vsock socket path, which ch50 fixes as `bays/<id>/vsock.sock`.
#[cfg(unix)]
static ENGINE: std::sync::Mutex<Option<Box<dyn crate::engine::Engine + Send>>> =
    std::sync::Mutex::new(None);

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
fn handle(mut conn: std::os::unix::net::UnixStream) -> anyhow::Result<()> {
    // byte-at-a-time line reads: an exec reply upgrades this same connection
    // to a raw byte relay, so read-ahead buffering is not allowed here.
    let home = bay_home()?;
    loop {
        let line = match crate::exec::read_line(&mut conn, None, 65536) {
            Ok(l) => l,
            Err(_) => return Ok(()), // client hung up
        };
        let req: Request = match serde_json::from_slice(&line) {
            Ok(req) => req,
            Err(_) => {
                reply(&mut conn, &Response::Err { error: "BAD_JSON".into() })?;
                return Ok(());
            }
        };
        let outcome = match req {
            Request::Up { n, .. } => up(&mut conn, n),
            Request::Exec { bay, argv } => exec_relay(&mut conn, &home, bay, &argv),
            other => reply(&mut conn, &dispatch(&other)).and(Ok(true)),
        };
        match outcome {
            Ok(keep_open) => {
                if !keep_open {
                    return Ok(()); // exec relayed: the connection is done
                }
            }
            Err(e) => {
                let _ = reply(&mut conn, &Response::Err { error: format!("{e}") });
                return Ok(());
            }
        }
    }
}

/// `bay up N`: boot bays 1..=N from the newest sealed golden. Reply ok when
/// all booted. ponytail: single golden pick (latest mtime); per-bay golden
/// pinning lands with halt/wake (#14's full fleet state is a later box).
#[cfg(unix)]
fn up(conn: &mut std::os::unix::net::UnixStream, n: u32) -> anyhow::Result<bool> {
    let home = bay_home()?;
    let gdir = latest_golden(&home.join("golden")).ok_or_else(|| {
        anyhow::anyhow!("NO_GOLDEN: bake one first (`bay bake <manifest.toml>`)")
    })?;
    let mut guard = ENGINE.lock().map_err(|_| anyhow::anyhow!("engine lock poisoned"))?;
    if guard.is_none() {
        *guard = Some(crate::engine::native_engine()?);
    }
    let golden = crate::engine::Golden { path: gdir };
    for i in 1..=n {
        guard.as_mut().unwrap().boot(i, &golden)?;
    }
    reply(conn, &Response::Ok)?;
    Ok(true)
}

/// Newest `golden/<hash>` dir holding a snapshot (the sealed contract);
/// None when no golden exists. Filesystem-only, so testable on every host.
pub fn latest_golden(golden_root: &Path) -> Option<PathBuf> {
    let rd = std::fs::read_dir(golden_root).ok()?;
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("snapshot").join("config.json").exists())
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
}

/// Exec upgrade: dial the bay's CH vsock socket, run the CONNECT 7
/// handshake (M0 finding), ack `ok`, then relay bytes until the guest
/// hangs up. Errors before the ack reply as normal protocol lines; after
/// it, a dropped connection is the only signal available (relay is raw).
#[cfg(unix)]
fn exec_relay(
    conn: &mut std::os::unix::net::UnixStream,
    home: &Path,
    bay: u32,
    argv: &[String],
) -> anyhow::Result<bool> {
    if argv.is_empty() {
        reply(conn, &Response::Err { error: "EMPTY_COMMAND".into() })?;
        return Ok(true);
    }
    let mut vsock = match std::os::unix::net::UnixStream::connect(crate::exec::vsock_path(home, bay)) {
        Ok(s) => s,
        Err(_) => {
            reply(conn, &Response::Err { error: "NO_BAY".into() })?;
            return Ok(true);
        }
    };
    let handshake = (|| -> anyhow::Result<()> {
        vsock.write_all(crate::exec::CONNECT)?;
        let ack = crate::exec::read_line(&mut vsock, Some(std::time::Duration::from_secs(10)), 4096)?;
        if !ack.starts_with(b"OK") {
            anyhow::bail!("guest agent refused CONNECT: {}", String::from_utf8_lossy(&ack));
        }
        Ok(())
    })();
    if let Err(e) = handshake {
        let _ = reply(
            conn,
            &Response::Err {
                error: format!("GUEST_UNREACHABLE: {e}"),
            },
        );
        return Ok(true);
    }
    reply(conn, &Response::Ok)?;
    crate::exec::pipe_both(conn, &mut vsock)?;
    Ok(false)
}

#[cfg(unix)]
fn reply(conn: &mut std::os::unix::net::UnixStream, resp: &Response) -> anyhow::Result<()> {
    conn.write_all(serde_json::to_string(resp)?.as_bytes())?;
    conn.write_all(b"\n")?;
    Ok(())
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::{UnixListener, UnixStream};

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

    #[test]
    fn pipe_both_relays_both_ways_and_propagates_eof() {
        // fake guest speaks the vsockd.c wire: CONNECT ack, echo the request
        // line back, sentinel, hang up. The client end must see both bytes
        // and a clean EOF, and the sentinel must still parse.
        let sock = std::env::temp_dir().join(format!("bayd-p-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let cli_l = UnixListener::bind(&sock).unwrap();
        let guest_l = UnixListener::bind(format!("{}g", sock.display())).unwrap();
        std::thread::spawn(|| {
            let (mut g, _) = guest_l.accept().unwrap();
            let first = crate::exec::read_line(&mut g, None, 64).unwrap();
            assert_eq!(first, b"CONNECT 7");
            g.write_all(b"OK 7\n").unwrap();
            let req = crate::exec::read_line(&mut g, None, 8192).unwrap();
            g.write_all(&req).unwrap();
            g.write_all(b"\n").unwrap();
            g.write_all(b"BAY_EXIT n5 5\n").unwrap();
        });
        std::thread::spawn(|| {
            let (mut cli, _) = cli_l.accept().unwrap();
            let mut vsock = UnixStream::connect(format!("{}g", sock.display())).unwrap();
            vsock.write_all(crate::exec::CONNECT).unwrap();
            let _ = crate::exec::read_line(&mut vsock, None, 64).unwrap();
            let _ = crate::exec::pipe_both(&mut cli, &mut vsock);
        });
        let mut client = UnixStream::connect(&sock).unwrap();
        client.write_all(b"payload\n").unwrap();
        let mut acc = Vec::new();
        client.read_to_end(&mut acc).unwrap();
        assert_eq!(acc, b"payload\n\nBAY_EXIT n5 5\n"); // relay is byte-faithful
        assert_eq!(crate::exec::strip_exit(&acc, "n5"), Some(5));
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(format!("{}g", sock.display()));
    }

    #[test]
    fn latest_golden_requires_sealed_snapshot() {
        let dir = std::env::temp_dir().join(format!("bayd-g-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(latest_golden(&dir), None); // absent root: no golden
        std::fs::create_dir_all(dir.join("ccc/snapshot")).unwrap();
        assert_eq!(latest_golden(&dir), None); // dir without config.json: not a golden
        std::fs::write(dir.join("ccc/snapshot/config.json"), b"{}").unwrap();
        assert_eq!(latest_golden(&dir), Some(dir.join("ccc")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
