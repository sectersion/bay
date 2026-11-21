//! Cloud Hypervisor v50 engine (M1c, #16). Linux runtime; compiles on all hosts.
//!
//! Pinned to CH v50, not newer: WSL2's Microsoft KVM lacks
//! `KVM_CAP_USER_MEMORY2`, which v55+ requires to boot (verified live in the
//! M0 spike, issue #1 verdict). Native Linux can run modern CH; revisit the
//! pin when WSL's KVM gains the cap.
//!
//! Drives CH entirely as a subprocess: one `ch-v50` per bay controlled through
//! its `--api-socket` via `ch-remote`. Fleet logic never sees this module
//! except through `Engine` (see `engine.rs`).
//!
//! # Golden contract
//!
//! A golden directory (`engine::Golden::path`) baked per the M1 bake flow:
//!
//! ```text
//! golden/
//!   disk.qcow2          base qcow2 disk (immutable; bay overlays CoW off it)
//!   overlay.tmpl.qcow2  fresh CH-compatible qcow2 overlay, backing = absolute
//!                       path of disk.qcow2 (created once by bake; boot just
//!                       copies it - no qemu-img at up time)
//!   snapshot/           CH snapshot: config.json, state.json, memory-ranges
//! ```
//!
//! `boot` copies the overlay template to `bays/<id>/disk.qcow2`, stages the
//! snapshot with per-bay paths rewritten into `config.json` (disk path and
//! host vsock socket; kernel/initramfs resolved to absolute at bake time),
//! then `--restore`s. Restore yields a PAUSED VM (v50 semantics), so boot
//! resumes it. Empirics (issue #1 kit, 2026-11-20): restore to vsock-socket
//! 75ms; CH v50's qcow2 reader requires qcow2-format backing files (a raw
//! backing file yields `BackingFileOpen(InvalidMagic)`).
//!
//! # Orphans
//!
//! Every child kill path ends in `Child::wait` (waitpid) so reaped children
//! cannot linger as zombies; `Drop` kills all still-running bays. A bayd that
//! dies uncleanly (SIGKILL) leaves its VMs; crash reconciliation is daemon
//! bookkeeping, not engine scope.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::engine::{Engine, Golden, Pty};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Ch50Engine {
    bays_dir: PathBuf,
    ch_bin: PathBuf,
    ch_remote: PathBuf,
    running: HashMap<u32, Running>,
}

struct Running {
    child: Child,
    api_sock: PathBuf,
    vsock_sock: PathBuf,
}

impl Ch50Engine {
    /// Engine over `~/.bay/bays` (or the given dir), discovering the CH
    /// binaries (see `discover`).
    pub fn new(bays_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let (ch_bin, ch_remote) = discover()?;
        Ok(Self {
            bays_dir: bays_dir.into(),
            ch_bin,
            ch_remote,
            running: HashMap::new(),
        })
    }

    fn bay_dir(&self, bay: u32) -> PathBuf {
        self.bays_dir.join(bay.to_string())
    }

    /// Spawn `--restore` on a snapshot dir, wait for the bay's vsock socket,
    /// resume, and register the child. Shared by boot (from golden) and
    /// resume (from the bay's own last halt).
    fn restore_and_resume(&mut self, bay: u32, snap_dir: &Path) -> anyhow::Result<Pty> {
        let dir = self.bay_dir(bay);
        let api = dir.join("api.sock");
        let vsock = dir.join("vsock.sock");
        // CH binds --api-socket before parsing --restore, and re-binds the
        // recorded vsock path; stale files from a dead bay would give phantom
        // EADDRINUSE (spike gotcha #3). Unlink first, ignore absence.
        unlink(&api)?;
        unlink(&vsock)?;

        let serial = std::fs::File::create(dir.join("serial.log"))?;
        let child = Command::new(&self.ch_bin)
            .args(boot_argv(&api, &file_url(snap_dir)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(serial.try_clone()?))
            .stderr(Stdio::from(serial))
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", self.ch_bin.display()))?;
        self.running.insert(
            bay,
            Running {
                child,
                api_sock: api.clone(),
                vsock_sock: vsock.clone(),
            },
        );

        if let Err(e) = wait_for_socket(&vsock, SOCKET_TIMEOUT) {
            let _ = self.kill_child(bay);
            return Err(e);
        }
        if let Err(e) = self.remote(bay, "resume", None) {
            let _ = self.kill_child(bay);
            return Err(e);
        }
        Ok(Pty { socket: vsock })
    }
    /// One ch-remote call against a running bay.
    fn remote(&self, bay: u32, verb: &str, arg: Option<&str>) -> anyhow::Result<()> {
        let api = self
            .running
            .get(&bay)
            .map(|r| r.api_sock.clone())
            .ok_or_else(|| anyhow::anyhow!("bay {bay} is not running"))?;
        run_remote(&self.ch_remote, &api, verb, arg)
    }

    /// SIGKILL + waitpid. Never leaves a zombie or a live VM behind.
    fn kill_child(&mut self, bay: u32) -> anyhow::Result<()> {
        if let Some(mut r) = self.running.remove(&bay) {
            let _ = r.child.kill();
            r.child
                .wait()
                .map_err(|e| anyhow::anyhow!("waitpid bay {bay}: {e}"))?;
        }
        Ok(())
    }
}

impl Engine for Ch50Engine {
    fn name(&self) -> &'static str {
        "ch50"
    }

    fn bake(&mut self, golden: &Golden) -> anyhow::Result<()> {
        let dir = &golden.path;
        std::fs::create_dir_all(dir.join("snapshot"))?;
        // Transient sockets live under /tmp with a pid suffix, NOT in the
        // golden dir: ~/.bay/golden/<64-hex>/bake-vsock.sock runs to ~109
        // chars and the AF_UNIX sun_path budget is 108 — CH fails to bind
        // and the VM dies at spawn (live-verified WSL, #15). The vsock path
        // is only recorded in snapshot config.json, which boot rewrites.
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let api = tmp.join(format!("bay-bake-{pid}.sock"));
        let vsock = tmp.join(format!("bay-bake-{pid}-vsock.sock"));
        let serial_log = dir.join("serial.log");
        unlink(&api)?;
        unlink(&vsock)?;
        let serial = std::fs::File::create(&serial_log)?;
        let mut vm = Command::new(&self.ch_bin)
            .args(bake_boot_argv(&api, &dir.join("kernel"), &dir.join("initramfs"), &dir.join("disk.qcow2"), &vsock))
            .stdin(Stdio::null())
            .stdout(Stdio::from(serial.try_clone()?))
            .stderr(Stdio::from(serial))
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", self.ch_bin.display()))?;
        // Cold boot to the guest's BAY_READY marker on the serial console
        // (~1.8s at 50M in M0; generous ceiling for loaded hosts).
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let log = std::fs::read(&serial_log).unwrap_or_default();
            if log.windows(9).any(|w| w == b"BAY_READY") {
                break;
            }
            if vm.try_wait().map_err(|e| anyhow::anyhow!("wait bake VM: {e}"))?.is_some()
                || Instant::now() >= deadline
            {
                let _ = vm.kill();
                let _ = vm.wait();
                anyhow::bail!("bake VM died or timed out before BAY_READY (see {})", serial_log.display());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let flow = (|| -> anyhow::Result<()> {
            run_remote(&self.ch_remote, &api, "pause", None)?;
            run_remote(&self.ch_remote, &api, "snapshot", Some(&file_url(&dir.join("snapshot"))))?;
            Ok(())
        })();
        // Always reap: a failed pause/snapshot must not orphan the VM.
        let _ = vm.kill();
        vm.wait().map_err(|e| anyhow::anyhow!("waitpid bake VM: {e}"))?;
        flow?;
        // Transient sockets are not part of the golden contract; the vsock
        // path recorded in snapshot config.json is rewritten per bay at boot.
        unlink(&api)?;
        unlink(&vsock)?;
        // DR-1: the overlay template (backing = base, absolute) is created
        // once here so `boot` is a 196KB copy with no qemu-img at up time.
        let out = Command::new("qemu-img")
            .args(create_overlay_argv(&dir.join("disk.qcow2"), &dir.join("overlay.tmpl.qcow2")))
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "qemu-img overlay: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn boot(&mut self, bay: u32, golden: &Golden) -> anyhow::Result<Pty> {
        if self.running.contains_key(&bay) || self.bay_dir(bay).exists() {
            anyhow::bail!("bay {bay} already exists (halt/resume it, or kill first)");
        }
        let overlay = golden.path.join("overlay.tmpl.qcow2");
        let snap = golden.path.join("snapshot");
        let cfg = std::fs::read(snap.join("config.json"))
            .map_err(|e| anyhow::anyhow!("read golden config: {e}"))?;

        let dir = self.bay_dir(bay);
        std::fs::create_dir_all(&dir)?;
        let staged = (|| -> anyhow::Result<()> {
            std::fs::copy(&overlay, dir.join("disk.qcow2"))?; // ~200KB, instant
            std::fs::create_dir_all(dir.join("snapshot"))?;
            // Big invariant blob: hardlink when possible, copy when not.
            let mr = snap.join("memory-ranges");
            if std::fs::hard_link(&mr, dir.join("snapshot/memory-ranges")).is_err() {
                std::fs::copy(&mr, dir.join("snapshot/memory-ranges"))?;
            }
            let st = snap.join("state.json");
            if std::fs::hard_link(&st, dir.join("snapshot/state.json")).is_err() {
                std::fs::copy(&st, dir.join("snapshot/state.json"))?;
            }
            let rewritten = rewrite_config(
                String::from_utf8_lossy(&cfg).as_ref(),
                &dir.join("disk.qcow2"),
                &dir.join("vsock.sock"),
                &golden.path,
            )?;
            std::fs::write(dir.join("snapshot/config.json"), rewritten)?;
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        self.restore_and_resume(bay, &dir.join("snapshot"))
    }

    fn halt(&mut self, bay: u32) -> anyhow::Result<()> {
        self.remote(bay, "pause", None)?;
        // CH wants the destination dir to pre-exist (spike gotcha); clear the
        // previous halt's snapshot first so the new one is authoritative.
        let snap = self.bay_dir(bay).join("snapshot");
        if snap.exists() {
            std::fs::remove_dir_all(&snap)?;
        }
        std::fs::create_dir_all(&snap)?;
        if let Err(e) = self.remote(bay, "snapshot", Some(&file_url(&snap))) {
            // VM is paused but not snapshotted; leave it running (paused) so
            // the caller can retry halt or kill. Parking only after the
            // kill below keeps "halted == snapshot exists" true.
            return Err(e);
        }
        self.kill_child(bay) // parked: zero RAM, files under bays/<id>/
    }

    fn resume(&mut self, bay: u32) -> anyhow::Result<Pty> {
        if self.running.contains_key(&bay) {
            anyhow::bail!("bay {bay} is already running");
        }
        let snap = self.bay_dir(bay).join("snapshot");
        if !snap.join("config.json").exists() {
            anyhow::bail!("bay {bay} has no snapshot to resume from");
        }
        self.restore_and_resume(bay, &snap)
    }

    fn kill(&mut self, bay: u32) -> anyhow::Result<()> {
        self.kill_child(bay)?;
        match std::fs::remove_dir_all(self.bay_dir(bay)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()), // idempotent
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Ch50Engine {
    fn drop(&mut self) {
        let bays: Vec<u32> = self.running.keys().copied().collect();
        for bay in bays {
            let _ = self.kill_child(bay);
        }
    }
}

fn unlink(p: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---- pure builders (unit-tested; also what the integration test reuses) ----

/// argv for the VM process: boot from a snapshot directory via restore.
pub(crate) fn boot_argv(api: &Path, snap_url: &str) -> Vec<String> {
    vec![
        "--api-socket".into(),
        api.display().to_string(),
        "--restore".into(),
        format!("source_url={snap_url}"),
    ]
}

/// argv for ch-remote: `--api-socket <api> <verb> [arg]`.
pub(crate) fn remote_argv(api: &Path, verb: &str, arg: Option<&str>) -> Vec<String> {
    let mut v = vec![
        "--api-socket".to_string(),
        api.display().to_string(),
        verb.to_string(),
    ];
    if let Some(a) = arg {
        v.push(a.to_string());
    }
    v
}

/// argv for the bake cold boot: direct kernel/initramfs boot of the golden
/// payload at 50M (matching bay sizing so the snapshot's memory-ranges are
/// bay-sized), serial on tty for the BAY_READY marker, vsock so the snapshot
/// freezes a live PTY listener. Same shape the #16 integration test proved.
pub(crate) fn bake_boot_argv(api: &Path, kernel: &Path, initramfs: &Path, disk: &Path, vsock: &Path) -> Vec<String> {
    vec![
        "--api-socket".into(), api.display().to_string(),
        "--kernel".into(), kernel.display().to_string(),
        "--initramfs".into(), initramfs.display().to_string(),
        "--disk".into(), format!("path={}", disk.display()),
        "--memory".into(), "size=50M".into(),
        "--cpus".into(), "boot=1".into(),
        "--console".into(), "off".into(),
        "--serial".into(), "tty".into(),
        "--vsock".into(), format!("cid=3,socket={}", vsock.display()),
        "--cmdline".into(), "console=ttyS0".into(),
    ]
}

/// argv creating the golden's fresh overlay template: qcow2 whose backing is
/// the base (qcow2 format mandated by CH v50's reader, DR-1).
pub(crate) fn create_overlay_argv(base: &Path, tmpl: &Path) -> Vec<String> {
    vec![
        "create".into(), "-f".into(), "qcow2".into(),
        "-b".into(), base.display().to_string(), "-F".into(), "qcow2".into(),
        tmpl.display().to_string(),
    ]
}

/// One ch-remote invocation against an explicit api socket (bake VM included).
fn run_remote(bin: &Path, api: &Path, verb: &str, arg: Option<&str>) -> anyhow::Result<()> {
    let out = Command::new(bin).args(remote_argv(api, verb, arg)).output()?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "ch-remote {verb}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// `file://` URL for CH snapshot/restore destinations. CH parses these as
/// URLs, so spaces (legal in $HOME on Linux) get the one percent-encode that
/// matters in practice; everything else stays verbatim.
pub(crate) fn file_url(p: &Path) -> String {
    format!("file://{}", p.display().to_string().replace(' ', "%20"))
}

/// Rewrite a golden snapshot's config.json for one bay: its CoW disk and its
/// host vsock socket. Relative payload paths (kernel/initramfs) resolve
/// against the golden dir so restore works from any CWD.
pub(crate) fn rewrite_config(
    config_json: &str,
    disk: &Path,
    vsock: &Path,
    payload_root: &Path,
) -> anyhow::Result<String> {
    let mut c: serde_json::Value = serde_json::from_str(config_json)?;
    set_ptr(&mut c, &["disks", "0", "path"], disk.display().to_string().into())?;
    set_ptr(
        &mut c,
        &["vsock", "socket"],
        vsock.display().to_string().into(),
    )?;
    for key in ["kernel", "initramfs"] {
        let abs = match c.get("payload").and_then(|p| p.get(key)) {
            Some(serde_json::Value::String(s)) if !s.is_empty() && !s.starts_with('/') => {
                Some(payload_root.join(s).display().to_string())
            }
            _ => None, // absolute or absent/null: leave as recorded
        };
        if let Some(v) = abs {
            set_ptr(&mut c, &["payload", key], v.into())?;
        }
    }
    Ok(serde_json::to_string(&c)?)
}

fn set_ptr(c: &mut serde_json::Value, ptr: &[&str], v: serde_json::Value) -> anyhow::Result<()> {
    let mut cur = c;
    for (i, k) in ptr.iter().enumerate() {
        let missing = || anyhow::anyhow!("golden config.json missing {}", ptr[..=i].join("."));
        cur = match (cur, k.parse::<usize>()) {
            // get_mut(&str) only walks objects; arrays need the numeric index
            (serde_json::Value::Array(a), Ok(n)) => a.get_mut(n).ok_or_else(missing)?,
            (obj, _) => obj.get_mut(*k).ok_or_else(missing)?,
        };
    }
    *cur = v;
    Ok(())
}

/// Locate the hypervisor binaries: `BAY_CH_BIN`/`BAY_CH_REMOTE` env override,
/// else `cloud-hypervisor`/`ch-remote` on PATH.
///
/// (Bundled download pinned by version+digest, per issue #16, is a follow-up;
/// the env override covers WSL kit layouts meanwhile.)
pub(crate) fn discover() -> anyhow::Result<(PathBuf, PathBuf)> {
    let ch = std::env::var_os("BAY_CH_BIN").map(PathBuf::from);
    let ch = ch.or_else(|| which("cloud-hypervisor"));
    let ch = ch.ok_or_else(|| {
        anyhow::anyhow!("cloud-hypervisor not found on PATH; install it or set BAY_CH_BIN")
    })?;
    let remote = std::env::var_os("BAY_CH_REMOTE").map(PathBuf::from);
    let remote = remote
        .or_else(|| which("ch-remote"))
        // v50 releases ship ch-remote alongside the binary
        .or_else(|| {
            let sib = ch.parent()?.join("ch-remote");
            sib.exists().then_some(sib)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("ch-remote not found; set BAY_CH_REMOTE (expected next to {ch:?})")
        })?;
    Ok((ch, remote))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|d| {
        let cand = d.join(name);
        cand.exists().then_some(cand)
    })
}

/// Wait for CH to (re)bind its host vsock socket. ~40-80ms in practice.
fn wait_for_socket(p: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if socket_ready(p) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for bay socket {}", p.display());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// CH rebinds the recorded vsock path; only accept a fresh socket, so a
/// leftover regular file can't fool us (unix: real socket check).
#[cfg(unix)]
fn socket_ready(p: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn socket_ready(p: &Path) -> bool {
    p.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_argv_exact() {
        assert_eq!(
            boot_argv(Path::new("/b/1/api.sock"), "file:///b/1/snap"),
            [
                "--api-socket",
                "/b/1/api.sock",
                "--restore",
                "source_url=file:///b/1/snap"
            ]
        );
    }

    #[test]
    fn remote_argv_exact() {
        assert_eq!(
            remote_argv(Path::new("/a.sock"), "snapshot", Some("file:///s")),
            ["--api-socket", "/a.sock", "snapshot", "file:///s"]
        );
        assert_eq!(
            remote_argv(Path::new("/a.sock"), "resume", None),
            ["--api-socket", "/a.sock", "resume"]
        );
    }

    #[test]
    fn paths_with_spaces_never_shell() {
        // Command() passes argv vectors without a shell; assert the builders
        // keep a spaced path as ONE argument and the URL encodes the space.
        let api = Path::new("/home/a b/bay 1/api.sock");
        let argv = boot_argv(api, &file_url(Path::new("/home/a b/snap")));
        assert_eq!(argv[1], "/home/a b/bay 1/api.sock");
        assert_eq!(argv[3], "source_url=file:///home/a%20b/snap");
    }

    #[test]
    fn rewrite_config_swaps_bay_paths() {
        let golden = r#"{"payload":{"kernel":"img/kata-kernel","initramfs":null,"cmdline":"c"},
          "disks":[{"path":"img/rootfs.ext4","readonly":false}],
          "vsock":{"cid":3,"socket":"vsock.sock"}}"#;
        let out = rewrite_config(
            golden,
            Path::new("/bays/1/disk.qcow2"),
            Path::new("/bays/1/vsock.sock"),
            Path::new("/golden/h"),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["disks"][0]["path"], "/bays/1/disk.qcow2");
        assert_eq!(v["vsock"]["socket"], "/bays/1/vsock.sock");
        // engine only runs on Linux, but the builder is unit-tested on every
        // host: compare against what Path::join produces, not a literal sep.
        assert_eq!(
            v["payload"]["kernel"],
            Path::new("/golden/h").join("img/kata-kernel").display().to_string()
        );
        assert_eq!(v["vsock"]["cid"], 3); // untouched
        assert!(v["payload"].get("initramfs").is_some()); // null preserved
    }

    #[test]
    fn rewrite_config_keeps_absolute_payload() {
        let golden = r#"{"payload":{"kernel":"/abs/k"},"disks":[{"path":"d"}],"vsock":{"socket":"s"}}"#;
        let out = rewrite_config(
            golden,
            Path::new("/d"),
            Path::new("/s"),
            Path::new("/golden"),
        )
        .unwrap();
        assert!(out.contains("\"kernel\":\"/abs/k\""));
    }

    #[test]
    fn rewrite_config_rejects_foreign_shape() {
        let e = rewrite_config(
            r#"{"disks":[]}"#,
            Path::new("/d"),
            Path::new("/s"),
            Path::new("/g"),
        )
        .unwrap_err();
        assert!(e.to_string().contains("disks.0"), "{e}");
    }

    #[test]
    fn file_url_absolute() {
        assert_eq!(
            file_url(Path::new("/root/x/snap")),
            "file:///root/x/snap"
        );
    }

    #[test]
    fn bake_boot_argv_exact() {
        let argv = bake_boot_argv(
            Path::new("/g/api.sock"),
            Path::new("/g/kernel"),
            Path::new("/g/initramfs"),
            Path::new("/g/disk.qcow2"),
            Path::new("/g/vsock.sock"),
        );
        assert_eq!(
            argv,
            [
                "--api-socket", "/g/api.sock",
                "--kernel", "/g/kernel",
                "--initramfs", "/g/initramfs",
                "--disk", "path=/g/disk.qcow2",
                "--memory", "size=50M",
                "--cpus", "boot=1",
                "--console", "off",
                "--serial", "tty",
                "--vsock", "cid=3,socket=/g/vsock.sock",
                "--cmdline", "console=ttyS0",
            ]
        );
    }

    #[test]
    fn create_overlay_argv_qcow2_backing() {
        assert_eq!(
            create_overlay_argv(Path::new("/g/disk.qcow2"), Path::new("/g/overlay.tmpl.qcow2")),
            ["create", "-f", "qcow2", "-b", "/g/disk.qcow2", "-F", "qcow2", "/g/overlay.tmpl.qcow2"]
        );
    }
}

/// Exit test for #16: two bays from one golden; a write in bay 1 must not
/// appear in bay 2 or the golden base; halt/wake round-trips; kill leaves no
/// orphans. Env-gated + `#[ignore]`d so CI never boots VMs; run on WSL2:
///
/// ```text
/// BAY_TEST_CH=/root/bay-m1c/bin/ch-v50 BAY_TEST_REMOTE=/root/bay-m1c/bin/ch-remote \
/// BAY_TEST_KERNEL=/root/bay-m1c/kata-kernel BAY_TEST_INITRD=/root/bay-m1c/initrd2 \
/// BAY_TEST_ROOTFS=/root/bay-m1c/rootfs.ext4 cargo test -- --ignored
/// ```
#[cfg(all(unix, test))]
pub(crate) mod integration {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    pub(crate) fn kit(k: &str) -> PathBuf {
        std::env::var_os(k)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("{k} must point at the WSL kit (see #16)"))
    }

    pub(crate) fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
        let out = Command::new(program).args(args).output()?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!(
                "{program} {args:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    }

    /// vsockd PTY: CONNECT 7, then feed `input`, drain until the guest hangs
    /// up (e.g. `exit`).
    pub(crate) fn pty(socket: &Path, input: &str) -> anyhow::Result<String> {
        let mut s = UnixStream::connect(socket)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(b"CONNECT 7\n")?;
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf)?; // "OK <port>"
        s.write_all(input.as_bytes())?;
        let mut acc = String::new();
        loop {
            match s.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => acc.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        Ok(acc)
    }

    fn contains(path: &Path, needle: &[u8]) -> anyhow::Result<bool> {
        let bytes = std::fs::read(path)?;
        Ok(bytes.windows(needle.len()).any(|w| w == needle))
    }

    /// ch-remote with an explicit api socket (the bake VM, before an Engine
    /// exists). v50 verbs: pause / snapshot <file://url> / resume.
    fn remote(api: &Path, verb: &str, arg: Option<&str>) -> anyhow::Result<()> {
        let mut cmd = Command::new(kit("BAY_TEST_REMOTE"));
        cmd.arg("--api-socket").arg(api).arg(verb);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        let out = cmd.output()?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!("{verb}: {}", String::from_utf8_lossy(&out.stderr).trim())
        }
    }

    #[test]
    #[ignore = "boots real VMs; needs WSL2 + BAY_TEST_{CH,REMOTE,KERNEL,INITRD,ROOTFS}"]
    fn two_bays_isolated_no_orphans() -> anyhow::Result<()> {
        let (ch, remote_bin) = (kit("BAY_TEST_CH"), kit("BAY_TEST_REMOTE"));
        std::env::set_var("BAY_CH_BIN", &ch);
        std::env::set_var("BAY_CH_REMOTE", &remote_bin);
        let root = std::env::temp_dir().join(format!("bay-m1c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let golden_dir = root.join("golden");
        std::fs::create_dir_all(golden_dir.join("snapshot"))?;
        let base = golden_dir.join("disk.qcow2");

        // Bake-ish: kit rootfs (raw) -> qcow2 base, cold boot, pause+snapshot
        // into golden/snapshot. (Real `bay bake` is a later issue; this
        // reproduces the golden contract the Engine expects.)
        run(
            "qemu-img",
            &[
                "convert",
                "-f",
                "raw",
                "-O",
                "qcow2",
                &kit("BAY_TEST_ROOTFS").display().to_string(),
                &base.display().to_string(),
            ],
        )?;
        let api = root.join("bake.sock");
        let bake_vsock = root.join("bake-vsock.sock");
        let serial_log = root.join("bake-serial.log");
        let serial = std::fs::File::create(&serial_log)?;
        let mut bake = Command::new(&ch)
            .arg("--api-socket")
            .arg(&api)
            .arg("--kernel")
            .arg(kit("BAY_TEST_KERNEL"))
            .arg("--initramfs")
            .arg(kit("BAY_TEST_INITRD"))
            .arg("--disk")
            .arg(format!("path={}", base.display()))
            .arg("--memory")
            .arg("size=50M")
            .arg("--cpus")
            .arg("boot=1")
            .arg("--console")
            .arg("off")
            .arg("--serial")
            .arg("tty")
            .arg("--vsock")
            .arg(format!("cid=3,socket={}", bake_vsock.display()))
            .arg("--cmdline")
            .arg("console=ttyS0")
            .stdin(Stdio::null())
            .stdout(Stdio::from(serial.try_clone()?))
            .stderr(Stdio::from(serial))
            .spawn()?;
        let t0 = Instant::now();
        loop {
            let log = std::fs::read(&serial_log).unwrap_or_default();
            if log.windows(9).any(|w| w == b"BAY_READY") {
                break;
            }
            if bake.try_wait()?.is_some() || t0.elapsed() > Duration::from_secs(90) {
                anyhow::bail!("bake VM died or timed out before BAY_READY");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let snap_url = file_url(&golden_dir.join("snapshot"));
        remote(&api, "pause", None)?;
        remote(&api, "snapshot", Some(&snap_url))?;
        let _ = bake.kill();
        bake.wait()?;

        // Overlay template: fresh qcow2 whose backing is the golden base.
        run(
            "qemu-img",
            &[
                "create",
                "-f",
                "qcow2",
                "-b",
                &base.display().to_string(),
                "-F",
                "qcow2",
                &golden_dir.join("overlay.tmpl.qcow2").display().to_string(),
            ],
        )?;

        // Two bays from one golden.
        let mut eng = Ch50Engine::new(root.join("bays"))?;
        let golden = Golden {
            path: golden_dir.clone(),
        };
        let p1 = eng.boot(1, &golden)?;
        let p2 = eng.boot(2, &golden)?;
        assert!(p1.socket != p2.socket, "bays must own distinct PTY sockets");

        // Write in bay 1 only.
        let mark = format!("M1CMARK-{}", std::process::id());
        pty(&p1.socket, &format!("echo {mark} > /m1c-marker; sync; exit\n"))?;

        // Park both.
        eng.halt(1)?;
        eng.halt(2)?;

        // Isolation: marker sits in bay 1's overlay, and in nothing else.
        let b1 = root.join("bays").join("1").join("disk.qcow2");
        let b2 = root.join("bays").join("2").join("disk.qcow2");
        assert!(
            contains(&b1, mark.as_bytes())?,
            "marker missing from bay 1 overlay"
        );
        assert!(
            !contains(&b2, mark.as_bytes())?,
            "marker leaked into bay 2 overlay"
        );
        assert!(
            !contains(&base, mark.as_bytes())?,
            "marker leaked into golden base (golden mutated!)"
        );

        // Wake bay 1: the marker must still read back through a fresh PTY.
        let p1 = eng.resume(1)?;
        let out = pty(&p1.socket, "cat /m1c-marker; exit\n")?;
        assert!(out.contains(&mark), "wake lost the marker: {out:?}");
        eng.halt(1)?;

        // Destroy; waitpid happens inside kill (no zombies, no live VMs).
        eng.kill(1)?;
        eng.kill(2)?;
        assert!(!root.join("bays").join("1").exists(), "kill left bay dir");
        assert!(!root.join("bays").join("2").exists(), "kill left bay dir");
        drop(eng);

        // No orphans: every spawn was paired with a wait. Match on the
        // process comm name — `pgrep -f` would match this test's own shell
        // wrapper, which carries the binary path in its argv (spike gotcha #2).
        let out = Command::new("pgrep")
            .args(["-x", ch.file_name().unwrap().to_str().unwrap()])
            .output()?;
        anyhow::ensure!(
            out.stdout.is_empty(),
            "orphaned CH processes: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        Ok(())
    }
}
