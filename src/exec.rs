//! Host side of `bay exec` (#17). Pure helpers (argv → guest wire, exit
//! sentinel) are unit-tested on all hosts; the relay itself is unix-only,
//! like bayd. Guest wire is documented in `guest/vsockd.c`.

use std::path::Path;

/// Shell-quote one argv element (POSIX single-quotes). `\\'`-trick, boring.
pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// First line to the guest agent: `<nonce> <shell command>\n`. The command
/// joins argv shell-quoted, so guest sees exactly the host argv (and a
/// caller wanting shell syntax passes one quoted arg or `bash -c ...`).
/// Rejects empty argv and embedded newlines (line-terminated wire).
pub fn first_line(nonce: &str, argv: &[String]) -> anyhow::Result<Vec<u8>> {
    if argv.is_empty() {
        anyhow::bail!("exec: empty command");
    }
    for a in argv {
        if a.contains('\n') {
            anyhow::bail!("exec: arguments containing newlines are not supported yet");
        }
    }
    let cmd = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!("{nonce} {cmd}\n").into_bytes())
}

/// Per-exec nonce: distinct across concurrent CLI processes and repeats.
pub fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", std::process::id(), nanos)
}

/// The guest agent's final sentinel: `buf` ends with
/// `BAY_EXIT <nonce> <code>` (plus optional trailing newlines). Returns the
/// body bytes (sentinel line and the agent's injected `\n` removed) and the
/// exit code. Nothing else on the stream can match: the nonce is per-exec
/// and the sentinel is guaranteed last (the agent writes it after waitpid,
/// then closes).
pub fn split_sentinel<'a>(buf: &'a [u8], nonce: &str) -> Option<(&'a [u8], i32)> {
    let t = trim_end_nl(buf);
    let line_start = t.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let prefix = format!("BAY_EXIT {nonce} ");
    let line = &t[line_start..];
    if !line.starts_with(prefix.as_bytes()) {
        return None;
    }
    let digits = &line[prefix.len()..];
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut body = &buf[..line_start];
    if body.last() == Some(&b'\n') {
        body = &body[..body.len() - 1]; // only the one injected separator
    }
    Some((body, String::from_utf8_lossy(digits).parse().ok()?))
}

/// Exit code if `buf` ends with the sentinel (unit-test surface).
pub fn strip_exit(buf: &[u8], nonce: &str) -> Option<i32> {
    split_sentinel(buf, nonce).map(|(_, c)| c)
}

/// Bytes of `buf` that belong to the guest's output (sentinel removed).
/// Falls back to the whole buffer when there is no sentinel.
pub fn strip_exit_body(buf: Vec<u8>, nonce: &str) -> (Vec<u8>, Option<i32>) {
    match split_sentinel(&buf, nonce) {
        Some((body, code)) => (body.to_vec(), Some(code)),
        None => (buf, None),
    }
}

fn trim_end_nl(mut b: &[u8]) -> &[u8] {
    while b.last() == Some(&b'\n') {
        b = &b[..b.len() - 1];
    }
    b
}

/// Max bytes `strip_exit_body` might hold back at stream end.
pub fn sentinel_hold(nonce: &str) -> usize {
    format!("\nBAY_EXIT {nonce} 255\n").len() + 8
}

/// CONNECT handshake the daemon issues against CH's per-bay vsock socket
/// (M0 finding, spike #1).
pub const CONNECT: &[u8] = b"CONNECT 7\n";

pub fn vsock_path(home: &Path, bay: u32) -> std::path::PathBuf {
    home.join("bays").join(bay.to_string()).join("vsock.sock")
}

#[cfg(unix)]
mod relay_impl {
    use super::*;
    use std::io::{IsTerminal, Read, Write};
    use std::os::unix::net::UnixStream;

    /// Copy both directions until EOF; used by bayd (cli↔vsock) and tested
    /// with an in-process fake guest. When either direction EOFs, the other
    /// side's write half is shut down so the peer sees the end too (guest
    /// EOF ⇒ cli sees EOF even while stdin is still open).
    pub fn pipe_both(cli: &mut UnixStream, guest: &mut UnixStream) -> anyhow::Result<()> {
        let mut g = guest.try_clone()?;
        let mut c2 = cli.try_clone()?;
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match g.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if c2.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = c2.shutdown(std::net::Shutdown::Write);
        });
        let mut buf = [0u8; 8192];
        loop {
            match cli.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if guest.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = guest.shutdown(std::net::Shutdown::Write);
        let _ = t.join();
        Ok(())
    }

    /// CLI half: put the real terminal in raw mode for the duration.
    /// `stty` (a stdlib-free syscall wrapper on every unix) instead of a
    /// libc dependency. Restores on drop, even on error paths.
    pub struct RawMode;
    impl RawMode {
        pub fn enter() -> Option<RawMode> {
            if !std::io::stdin().is_terminal() {
                return None;
            }
            let r = std::process::Command::new("stty")
                .args(["raw", "-echo"])
                .status()
                .ok()?;
            r.success().then_some(RawMode)
        }
    }
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = std::process::Command::new("stty").arg("sane").status();
        }
    }

    /// `bay exec <bay> -- <cmd>`, end to end: dial bayd, get the upgrade
    /// ack, then relay the terminal against the guest agent through it.
    pub fn run(bay: u32, argv: &[String]) -> anyhow::Result<i32> {
        let req = crate::protocol::Request::Exec {
            bay,
            argv: argv.to_vec(),
        };
        let sock = crate::daemon::sock_path()?;
        let mut sock_conn = UnixStream::connect(&sock).map_err(|_| {
            anyhow::anyhow!("cannot reach bayd at {} — start it with `bay daemon`", sock.display())
        })?;
        let sock = &mut sock_conn;
        writeln!(sock, "{}", serde_json::to_string(&req)?)?;
        // one byte at a time: the ack is exactly one line, and the raw
        // stream that follows must not be swallowed by a read-ahead buffer.
        let line = read_line(sock, Some(std::time::Duration::from_secs(10)), 4096)?;
        let resp: crate::protocol::Response = serde_json::from_slice(&line)?;
        if let crate::protocol::Response::Err { error } = resp {
            anyhow::bail!("bayd: {error}");
        }
        // the ack was bounded; the relay is not
        sock.set_read_timeout(None)?;
        let n = nonce();
        sock.write_all(&first_line(&n, argv)?)?;

        let _raw = RawMode::enter();
        let mut inbound = sock.try_clone()?;
        let stdin_t = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut stdin = std::io::stdin();
            while let Ok(r) = stdin.read(&mut buf) {
                if r == 0 || inbound.write_all(&buf[..r]).is_err() {
                    break;
                }
            }
        });
        let hold = sentinel_hold(&n);
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let r = sock.read(&mut buf)?;
            if r == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..r]);
            if tail.len() > hold {
                std::io::Write::write_all(&mut std::io::stdout(), &tail[..tail.len() - hold])?;
                std::io::Write::flush(&mut std::io::stdout())?;
                tail.drain(..tail.len() - hold);
            }
        }
        drop(_raw);
        let (body, code) = strip_exit_body(tail, &n);
        let _ = stdin_t.join();
        std::io::Write::write_all(&mut std::io::stdout(), &body)?;
        std::io::Write::flush(&mut std::io::stdout())?;
        code.ok_or_else(|| anyhow::anyhow!("relay closed with no exit sentinel (guest agent died?)"))
    }

    /// Byte-at-a-time line read — never over-reads, so the same connection
    /// can carry raw bytes after the line (exec upgrade, CONNECT ack).
    pub fn read_line(
        s: &mut UnixStream,
        timeout: Option<std::time::Duration>,
        cap: usize,
    ) -> anyhow::Result<Vec<u8>> {
        s.set_read_timeout(timeout)?;
        let mut acc = Vec::new();
        let mut b = [0u8; 1];
        loop {
            let n = s.read(&mut b)?;
            if n == 0 {
                anyhow::bail!("connection closed");
            }
            if b[0] == b'\n' || acc.len() >= cap {
                return Ok(acc);
            }
            acc.push(b[0]);
        }
    }
}

#[cfg(unix)]
pub use relay_impl::{pipe_both, read_line, run};

#[cfg(not(unix))]
pub fn run(_bay: u32, _argv: &[String]) -> anyhow::Result<i32> {
    anyhow::bail!("{}", crate::daemon::WINDOWS_MSG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_boring_and_nasty() {
        assert_eq!(shell_quote("hi"), "'hi'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn first_line_wire() {
        assert_eq!(
            String::from_utf8(first_line("abc", &["echo".into(), "hi".into()]).unwrap()).unwrap(),
            "abc 'echo' 'hi'\n"
        );
        // quoting is per-arg: shell metacharacters from a caller who passed
        // a single arg stay inert (use `bash -c` deliberately for those)
        assert!(String::from_utf8_lossy(&first_line("n", &["a;b".into()]).unwrap()).contains("'a;b'"));
        assert!(first_line("n", &[]).is_err());
        assert!(first_line("n", &["two\nlines".into()]).is_err());
    }

    #[test]
    fn exit_sentinel() {
        let buf = b"hi\r\n\nBAY_EXIT abc123 0\n";
        assert_eq!(strip_exit(buf, "abc123"), Some(0));
        assert_eq!(strip_exit(b"no mark\n", "abc123"), None);
        // wrong nonce must not match
        assert_eq!(strip_exit(buf, "other"), None);
        // code 127, no trailing newline, command output without newline
        assert_eq!(strip_exit(b"x\nBAY_EXIT n 127", "n"), Some(127));
        // body strip: output kept, injected \n + sentinel gone
        let (body, code) = strip_exit_body(b"hi\r\n\nBAY_EXIT n 3\n".to_vec(), "n");
        assert_eq!((body.as_slice(), code), (&b"hi\r\n"[..], Some(3)));
        // no sentinel => body passes through untouched
        let (body, code) = strip_exit_body(b"partial".to_vec(), "n");
        assert_eq!((body.as_slice(), code), (&b"partial"[..], None));
    }

    #[test]
    fn nonce_unique_and_short() {
        let a = nonce();
        assert!(a.len() < 32 && !a.contains(' ') && !a.is_empty());
    }

    #[test]
    fn vsock_path_shape() {
        let p = vsock_path(Path::new("/h"), 2);
        assert_eq!(p, Path::new("/h").join("bays").join("2").join("vsock.sock"));
    }
}

/// Exit test for #17, verbatim from the issue: `bay up 2`, `bay exec 1 --
/// echo hi` prints hi + exit 0, exec runs concurrently in both bays, exit
/// codes propagate, and the guest really is on a PTY. Needs the WSL2 kit
/// (#15/#16) plus a vsockd built from `guest/vsockd.c` (built on the fly
/// with gcc when BAY_TEST_VSOCKD is unset):
///
/// ```text
/// BAY_TEST_CH=/root/bay-spike/bin/ch-v50 BAY_TEST_REMOTE=/root/bay-spike/bin/ch-remote \
/// BAY_TEST_KERNEL=/root/bay-spike/img/kata-kernel BAY_TEST_INITRD=/root/bay-spike/img/initrd2 \
/// BAY_TEST_ROOTFS=/root/bay-spike/img/rootfs.ext4 cargo test -- --ignored
/// ```
#[cfg(all(unix, test))]
mod integration {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn bin() -> PathBuf {
        std::env::var_os("BAY_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/bay"))
    }

    fn cli(home: &PathBuf, args: &[&str]) -> std::process::Output {
        assert!(
            bin().exists(),
            "CLI binary {} missing — `cargo build` first (or set BAY_BIN), see bake.rs integration docs",
            bin().display()
        );
        Command::new(bin())
            .args(args)
            .env("BAY_HOME", home)
            .output()
            .expect("run bay CLI")
    }

    #[test]
    #[ignore = "boots real VMs; needs WSL2 root + #15/#16 kit env (see module docs)"]
    fn exec_end_to_end() -> anyhow::Result<()> {
        let root = Path::new("/tmp").join(format!("bay-w17-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        let home = root.join("home");
        std::env::set_var("BAY_HOME", &home);
        std::env::set_var("BAY_CH_BIN", crate::ch50::integration::kit("BAY_TEST_CH"));
        std::env::set_var("BAY_CH_REMOTE", crate::ch50::integration::kit("BAY_TEST_REMOTE"));

        // build the tracked guest agent if no prebuilt one was given
        let vsockd = match std::env::var_os("BAY_TEST_VSOCKD") {
            Some(v) => PathBuf::from(v),
            None => {
                let out = root.join("vsockd");
                let c = Command::new("gcc")
                    .args(["-O2", "-static", "-o"])
                    .arg(&out)
                    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("guest/vsockd.c"))
                    .arg("-lutil")
                    .output()?;
                anyhow::ensure!(c.status.success(), "gcc vsockd: {}", String::from_utf8_lossy(&c.stderr));
                out
            }
        };

        // bake a golden carrying the new agent (agents copy to
        // /usr/local/bin; kit /init starts /usr/local/sbin/vsockd → symlink)
        std::fs::write(
            root.join("setup.sh"),
            "ln -sf /usr/local/bin/vsockd /usr/local/sbin/vsockd\n",
        )?;
        let mp = root.join("golden.toml");
        std::fs::write(
            &mp,
            format!(
                "kernel = {:?}\ninitramfs = {:?}\nbase_rootfs = {:?}\nscripts = [\"setup.sh\"]\nagents = [{:?}]\n",
                crate::ch50::integration::kit("BAY_TEST_KERNEL"),
                crate::ch50::integration::kit("BAY_TEST_INITRD"),
                crate::ch50::integration::kit("BAY_TEST_ROOTFS"),
                vsockd,
            ),
        )?;
        let baked = crate::bake::run(&mp)?;
        assert!(baked.baked);

        // daemon up (its own BAY_HOME), then the exit test itself
        assert!(bin().exists(), "CLI binary {} missing — cargo build first", bin().display());
        let mut daemon = Command::new(bin())
            .arg("daemon")
            .env("BAY_HOME", &home)
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let sock = home.join("bayd.sock");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::ensure!(sock.exists(), "bayd never bound {}", sock.display());

        let up = cli(&home, &["up", "2"]);
        anyhow::ensure!(up.status.success(), "up 2: {}", String::from_utf8_lossy(&up.stderr));

        // 1. the exit test verbatim
        let out = cli(&home, &["exec", "1", "--", "echo", "hi"]);
        let text = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        anyhow::ensure!(
            out.status.code() == Some(0) && text.contains("hi") && !text.contains("BAY_EXIT"),
            "exec echo hi: exit={:?} out={text:?}",
            out.status.code()
        );
        println!("EXEC1: {text:?} exit={:?}", out.status.code());

        // 2. concurrent exec in both bays (both live VMs relaying at once)
        let a = Command::new(bin())
            .args(["exec", "1", "--", "echo", "A"])
            .env("BAY_HOME", &home)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let b = Command::new(bin())
            .args(["exec", "2", "--", "echo", "B"])
            .env("BAY_HOME", &home)
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let (a, b) = (a.wait_with_output()?, b.wait_with_output()?);
        anyhow::ensure!(
            a.status.success() && b.status.success()
                && String::from_utf8_lossy(&a.stdout).contains('A')
                && String::from_utf8_lossy(&b.stdout).contains('B'),
            "concurrent: A={a:?} B={b:?}"
        );
        println!("CONCURRENT: ok");

        // 3. exit code propagation
        let out = cli(&home, &["exec", "1", "--", "sh", "-c", "exit 7"]);
        anyhow::ensure!(
            out.status.code() == Some(7) && !String::from_utf8_lossy(&out.stdout).contains("BAY_EXIT"),
            "exit 7: got {:?}",
            out.status.code()
        );
        println!("EXITCODE: ok");

        // 4. PTY fidelity (cheap): guest stdout really is a pty, and a
        //    shell builtin pipeline works through the quoted wire
        let out = cli(&home, &["exec", "1", "--", "tty"]);
        anyhow::ensure!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("/dev/pts/"),
            "tty: {}", String::from_utf8_lossy(&out.stdout)
        );
        println!("PTY: {}", String::from_utf8_lossy(&out.stdout).trim());

        // 5. unknown bay
        let out = cli(&home, &["exec", "99", "--", "echo", "x"]);
        anyhow::ensure!(!out.status.success(), "exec on missing bay must fail");

        daemon.kill()?;
        daemon.wait()?;
        let _ = Command::new("killall").args(["-9", "ch-v50", "cloud-hypervisor", "ch-remote"]).output();
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
