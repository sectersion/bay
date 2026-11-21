//! Thin CLI client: connect to bayd, send one request, read one reply.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::path::Path;

use crate::protocol::{Request, Response};

#[cfg(unix)]
pub fn request(req: &Request) -> anyhow::Result<Response> {
    round_trip(&crate::daemon::sock_path()?, req)
}

#[cfg(not(unix))]
pub fn request(req: &Request) -> anyhow::Result<Response> {
    let _ = req;
    anyhow::bail!("{}", crate::daemon::WINDOWS_MSG)
}

#[cfg(unix)]
pub(crate) fn round_trip(sock: &Path, req: &Request) -> anyhow::Result<Response> {
    let mut stream = std::os::unix::net::UnixStream::connect(sock)
        .map_err(|_| anyhow::anyhow!("cannot reach bayd at {} — start it with `bay daemon`", sock.display()))?;
    writeln!(stream, "{}", serde_json::to_string(req)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("bayd closed the connection");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}
