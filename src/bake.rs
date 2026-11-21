//! Golden bake (M1b, #15): manifest → content-hash dir under
//! `~/.bay/golden/<hash>/` containing the full golden contract (see
//! `ch50` module docs): kernel, initramfs, disk.qcow2, overlay.tmpl.qcow2,
//! snapshot/, manifest.toml. A golden is immutable once written; a re-bake
//! whose hash dir already exists is a no-op.
//!
//! Manifest format (subset of TOML, parsed with `toml` + serde):
//!
//! ```toml
//! kernel      = "/root/bay-spike/img/kata-kernel"   # required
//! initramfs   = "/root/bay-spike/img/initrd2"       # required
//! base_rootfs = "/root/bay-spike/img/rootfs.ext4"   # required (raw ext4)
//! packages    = []                                   # apt pkgs, chroot-installed
//! scripts     = ["setup.sh"]                         # host files, run in chroot (bash), in order
//! agents      = []                                   # host binaries copied to /usr/local/bin/
//! ```
//!
//! Relative paths resolve against the manifest's directory. The content hash
//! covers the manifest bytes plus the full contents of every referenced file
//! (kernel, initramfs, base rootfs, scripts, agents) in fixed field order —
//! deliberately conservative: any manifest edit (even a kit path move)
//! re-bakes; stale kit *contents* under an unchanged path re-bake too,
//! because file bytes are in the hash.
//!
//! Bake currently runs in the CLI process (`bay bake`), not through bayd:
//! `protocol::dispatch` is stateless today and the daemon holds no engine
//! yet; wiring Bake through the socket is a one-line move when it does.
//! Unknown manifest keys are rejected — a typo'd input must not silently
//! bake a wrong golden.

use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::io::ErrorKind;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Everything `bay bake` outputs; printed as JSON by the CLI.
#[derive(Serialize, Debug)]
pub struct Outcome {
    pub hash: String,
    pub dir: PathBuf,
    /// false = hash dir already existed; nothing was done.
    pub baked: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    pub base_rootfs: PathBuf,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub scripts: Vec<PathBuf>,
    #[serde(default)]
    pub agents: Vec<PathBuf>,
}

impl Manifest {
    /// Parse a manifest file; resolves relative input paths against the
    /// manifest's own directory.
    pub fn load(path: &Path) -> anyhow::Result<Manifest> {
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read manifest {}: {e}", path.display()))?;
        let m: Manifest = toml::from_str(&String::from_utf8_lossy(&bytes))
            .map_err(|e| anyhow::anyhow!("parse manifest {}: {e}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let resolve = |p: PathBuf| -> PathBuf {
            if p.is_absolute() { p } else { base.join(p) }
        };
        Ok(Manifest {
            kernel: resolve(m.kernel),
            initramfs: resolve(m.initramfs),
            base_rootfs: resolve(m.base_rootfs),
            packages: m.packages,
            scripts: m.scripts.into_iter().map(resolve).collect(),
            agents: m.agents.into_iter().map(resolve).collect(),
        })
    }

    fn input_paths(&self) -> Vec<&Path> {
        let mut v = vec![self.kernel.as_path(), self.initramfs.as_path(), self.base_rootfs.as_path()];
        v.extend(self.scripts.iter().map(PathBuf::as_path));
        v.extend(self.agents.iter().map(PathBuf::as_path));
        v
    }

    /// sha256 over manifest bytes + every referenced file's contents, in a
    /// fixed order, length-prefixed so concatenations can't collide.
    pub fn content_hash(&self, manifest_bytes: &[u8]) -> anyhow::Result<String> {
        let mut h = Sha256::new();
        h.update(b"bay-golden-v1");
        feed(&mut h, manifest_bytes);
        for p in self.input_paths() {
            let bytes = std::fs::read(p)
                .map_err(|e| anyhow::anyhow!("hash input {}: {e}", p.display()))?;
            feed(&mut h, &bytes);
        }
        Ok(hex(&h.finalize()))
    }
}

fn feed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn hex(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `~/.bay/golden/<hash>` for a hash.
pub fn golden_dir(home: &Path, hash: &str) -> PathBuf {
    home.join("golden").join(hash)
}

/// Sibling completion marker: present while a bake is in flight (or died
/// mid-flight). Lives OUTSIDE the golden dir so a completed golden dir holds
/// only contract files.
fn marker(home: &Path, hash: &str) -> PathBuf {
    home.join("golden").join(format!(".{hash}.incomplete"))
}

/// A hash dir is a hit only if it exists and no in-flight marker sits beside
/// it. A crashed bake leaves the marker → next bake wipes and rebuilds.
pub fn golden_exists(home: &Path, hash: &str) -> bool {
    golden_dir(home, hash).exists() && !marker(home, hash).exists()
}

pub fn run(manifest_path: &Path) -> anyhow::Result<Outcome> {
    #[cfg(target_os = "linux")]
    { run_linux(manifest_path) }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = manifest_path;
        anyhow::bail!("golden bake needs Linux; run `bay bake` under WSL2 (see #15)")
    }
}

/// argv builders — pure, unit-tested on all hosts; the Linux flow runs them
/// via Command without a shell.
pub(crate) fn mount_loop_argv(img: &Path, dir: &Path) -> Vec<String> {
    vec!["mount".into(), "-o".into(), "loop".into(), img.display().to_string(), dir.display().to_string()]
}
pub(crate) fn bind_argv(src: &str, dst: &Path) -> Vec<String> {
    // plain --bind, NOT --rbind: rbind drags cgroup2/hugetlbfs submounts in
    // with shared propagation and umount dies "target is busy" (live-verified
    // on WSL). apt only needs /dev/null + kernel sysfs itself, never the
    // pseudo-fs children.
    vec!["mount".into(), "--bind".into(), src.into(), dst.display().to_string()]
}
pub(crate) fn umount_argv(dir: &Path) -> Vec<String> {
    vec!["umount".into(), dir.display().to_string()]
}
pub(crate) fn umount_lazy_argv(dir: &Path) -> Vec<String> {
    vec!["umount".into(), "-l".into(), dir.display().to_string()]
}
pub(crate) fn apt_install_argv(mnt: &Path, pkgs: &[String]) -> Vec<String> {
    let mut v = vec![
        "chroot".into(), mnt.display().to_string(),
        "apt-get".into(), "-y".into(), "-q".into(), "install".into(),
        "--no-install-recommends".into(),
    ];
    v.extend(pkgs.iter().cloned());
    v
}
pub(crate) fn script_argv(mnt: &Path, guest_path: &str) -> Vec<String> {
    vec!["chroot".into(), mnt.display().to_string(), "bash".into(), guest_path.into()]
}
pub(crate) fn convert_argv(raw: &Path, qcow2: &Path) -> Vec<String> {
    vec![
        "qemu-img".into(), "convert".into(),
        "-f".into(), "raw".into(), "-O".into(), "qcow2".into(),
        raw.display().to_string(), qcow2.display().to_string(),
    ]
}

#[cfg(target_os = "linux")]
fn run_linux(manifest_path: &Path) -> anyhow::Result<Outcome> {
    use crate::engine;

    let manifest_bytes = std::fs::read(manifest_path)
        .map_err(|e| anyhow::anyhow!("read manifest {}: {e}", manifest_path.display()))?;
    let m = Manifest::load(manifest_path)?;
    let hash = m.content_hash(&manifest_bytes)?;
    let home = crate::daemon::bay_home()?;
    let gdir = golden_dir(&home, &hash);
    if golden_exists(&home, &hash) {
        return Ok(Outcome { hash, dir: gdir, baked: false });
    }
    // Crashed bake left a dir + marker: wipe (marker path implies dir may
    // exist half-written). A SIGKILL inside the chroot/mount window leaves
    // gdir/mnt loop-mounted, and remove_dir_all on a mounted subtree is
    // EBUSY forever — retry wedges (#28 round-1 blocker). Tear the mount
    // subtree down first, best-effort, then remove; sweep dead bakers'
    // transient /tmp sockets while we're recovering.
    if gdir.exists() {
        let _ = teardown_mounts(&gdir, false);
        sweep_stale_bake_sockets();
        std::fs::remove_dir_all(&gdir)?;
    }
    std::fs::create_dir_all(&home.join("golden"))?;
    let mk = marker(&home, &hash);
    std::fs::write(&mk, b"")?;

    let built = (|| -> anyhow::Result<()> {
        std::fs::create_dir_all(&gdir)?;
        std::fs::write(gdir.join("manifest.toml"), &manifest_bytes)?;
        std::fs::copy(&m.kernel, gdir.join("kernel"))?;
        std::fs::copy(&m.initramfs, gdir.join("initramfs"))?;
        build_disk(&m, &gdir)?;
        let mut eng = engine::native_engine()?;
        let golden = engine::Golden { path: gdir.clone() };
        eng.bake(&golden)
    })();
    if let Err(e) = built {
        // Deliberately leave the partial dir + marker: golden_exists() still
        // says no (next run wipes and rebuilds), and the failed bake's
        // serial.log survives for diagnosis.
        return Err(e.context(format!(
            "bake left partial golden at {} (wiped on retry)",
            gdir.display()
        )));
    }
    seal_immutable(&gdir)?;
    match std::fs::remove_file(&mk) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(Outcome { hash, dir: gdir, baked: true })
}

/// base ext4 → golden disk.qcow2, with packages/scripts/agents applied in a
/// loop-mounted chroot. Skips the mount entirely when there is nothing to
/// install (pure convert).
#[cfg(target_os = "linux")]
fn build_disk(m: &Manifest, gdir: &Path) -> anyhow::Result<()> {
    let modify = !m.packages.is_empty() || !m.scripts.is_empty() || !m.agents.is_empty();
    let raw = gdir.join("rootfs.raw");
    let disk = gdir.join("disk.qcow2");
    if !modify {
        // No edits: convert base directly. (A golden without a writable
        // staging copy is still CoW-safe: bays overlay off disk.qcow2.)
        run_cmd(&convert_argv(&m.base_rootfs, &disk))?;
        return Ok(());
    }
    std::fs::copy(&m.base_rootfs, &raw)?;
    let mnt = gdir.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    // Teardown of everything under gdir goes through the shared
    // teardown_mounts guard (mountinfo-driven, so a SIGKILL can't strand a
    // loop mount that the retry's recovery then chokes on forever — #28
    // round-1 blocker), never through an in-memory stack that dies with the
    // process.
    let staged = (|| -> anyhow::Result<()> {
        run_cmd(&mount_loop_argv(&raw, &mnt))?;
        for src in ["/dev", "/proc", "/sys"] {
            let dst = mnt.join(src.trim_start_matches('/'));
            run_cmd(&bind_argv(src, &dst))?;
        }
        let _ = std::fs::copy("/etc/resolv.conf", mnt.join("etc/resolv.conf"));
        if !m.packages.is_empty() {
            run_cmd(&apt_install_argv(&mnt, &m.packages))?;
        }
        for (i, s) in m.scripts.iter().enumerate() {
            let guest = format!("/tmp/bay-setup-{}.sh", i);
            // Path::join with a leading-slash guest path would REPLACE mnt —
            // strip it to land inside the mounted rootfs.
            let host = mnt.join(guest.trim_start_matches('/'));
            std::fs::copy(s, &host)?;
            let r = run_cmd(&script_argv(&mnt, &guest));
            let _ = std::fs::remove_file(&host);
            r?;
        }
        let bin = mnt.join("usr/local/bin");
        std::fs::create_dir_all(&bin)?;
        for a in &m.agents {
            let dst = bin.join(a.file_name().ok_or_else(|| anyhow::anyhow!("agent path has no file name: {}", a.display()))?);
            std::fs::copy(a, &dst)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = teardown_mounts(gdir, false);
        let _ = std::fs::remove_dir_all(&mnt);
        return Err(e);
    }
    // Eager: the qcow2 convert must not read the raw image before its
    // mounts are really gone (unflushed chroot writes would be lost).
    teardown_mounts(gdir, true)?;
    std::fs::remove_dir_all(&mnt)?;
    run_cmd(&convert_argv(&raw, &disk))?;
    std::fs::remove_file(&raw)?;
    Ok(())
}

/// Best-effort reverse-order teardown of every mount at or under `gdir`,
/// plus detach of the loop devices backing it. Reads the kernel's view
/// (`/proc/self/mountinfo`) instead of any in-memory stack, so it is
/// correct for a mount a SIGKILLed bake left behind. `eager` = fail on the
/// first umount error (success path: convert must wait for real unmounts);
/// otherwise ignore all errors — cleanup on a failure or recovery path is
/// never allowed to wedge the retry it is enabling. Does not remove
/// directories; callers do their own removal after this.
#[cfg(target_os = "linux")]
fn teardown_mounts(gdir: &Path, eager: bool) -> anyhow::Result<()> {
    let prefix = gdir.display().to_string();
    let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    for p in mounts_under(&info, &prefix) {
        let mnt = Path::new(&p);
        match run_cmd(&umount_argv(mnt)) {
            Ok(()) => {}
            Err(e) => {
                if eager {
                    return Err(e);
                }
                // A SIGKILLed bake can leave chroot occupants alive for a
                // moment; plain umount says EBUSY. Lazy detach makes the
                // mountpoint vanish from the namespace immediately, which
                // is all remove_dir_all needs — and recovery re-bakes
                // from the base rootfs anyway, so unflushed staging data
                // is intentionally discarded here.
                let _ = run_cmd(&umount_lazy_argv(mnt));
            }
        }
    }
    for dev in loop_backing(&info, &prefix) {
        let _ = run_cmd(&["losetup".into(), "-d".into(), dev]);
    }
    // Loop attached with no mount (bake killed between attach and mount in
    // `mount -o loop`): only findable via the backing file.
    let raw = gdir.join("rootfs.raw");
    if let Some(out) = stdout_of(&["losetup".into(), "-j".into(), raw.display().to_string()]) {
        for dev in losetup_devices(&out) {
            let _ = run_cmd(&["losetup".into(), "-d".into(), dev]);
        }
    }
    Ok(())
}

/// Reap /tmp sockets a dead bake left behind: names from OUR scheme only
/// (`bay-bake-{pid}[-vsock].sock`), removed only when the embedded pid no
/// longer exists in /proc. Foreign /tmp files are never touched.
#[cfg(target_os = "linux")]
fn sweep_stale_bake_sockets() {
    let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for e in rd.flatten() {
        let Some(pid) = bake_socket_pid(&e.file_name().to_string_lossy()) else { continue };
        // ponytail: pid-reuse can spare a stale socket (harmless — a live
        // CH re-binds its own on exit; worst case one dead file lingers a
        // little longer).
        if proc_alive(Path::new("/proc"), pid) {
            continue;
        }
        let _ = std::fs::remove_file(e.path());
    }
}

#[cfg(target_os = "linux")]
fn stdout_of(argv: &[String]) -> Option<String> {
    let out = std::process::Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---- pure mountinfo/socket helpers: compiled + unit-tested on all CI hosts ----

/// Undo mountinfo's octal escapes (`\040` space, `\011` tab, `\012`
/// newline, `\134` backslash); a stray backslash passes through.
fn unescape_mount(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let digits = &rest[i + 1..];
        if digits.len() >= 3 && digits.as_bytes()[..3].iter().all(|b| *b >= b'0' && *b <= b'7') {
            if let Ok(v) = u8::from_str_radix(&digits[..3], 8) {
                out.push(if v == 0x5c { '\\' } else { v as char });
                rest = &digits[3..];
                continue;
            }
        }
        out.push('\\');
        rest = &rest[i + 1..];
    }
    out.push_str(rest);
    out
}

/// Mount points at or under `prefix`, deepest-first (children unmounted
/// before parents). `mountinfo` is the raw text of /proc/self/mountinfo.
pub(crate) fn mounts_under(mountinfo: &str, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let mp = unescape_mount(*f.get(4)?);
            (mp == prefix || mp.starts_with(&format!("{prefix}/"))).then_some(mp)
        })
        .collect();
    v.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    v
}

/// `/dev/loopN` devices backing mounts at or under `prefix`, deduped.
pub(crate) fn loop_backing(mountinfo: &str, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = mountinfo
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let sep = f.iter().position(|t| *t == "-")?;
            let mp = unescape_mount(*f.get(4)?);
            let src = f.get(sep + 2)?;
            (src.starts_with("/dev/loop")
                && (mp == prefix || mp.starts_with(&format!("{prefix}/"))))
                .then(|| src.to_string())
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Device names in `losetup -j <path>` output (lines like
/// `/dev/loopN: [maj:min]:ino (path)`).
pub(crate) fn losetup_devices(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| {
            l.split(':').next().filter(|d| d.starts_with("/dev/loop")).map(String::from)
        })
        .collect()
}

/// Pid embedded in one of OUR transient bake socket names, `None` for every
/// other file name — the sweep never claims foreign /tmp files.
pub(crate) fn bake_socket_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("bay-bake-")?;
    let pid = rest.strip_suffix("-vsock.sock").or_else(|| rest.strip_suffix(".sock"))?;
    pid.parse().ok()
}

/// True if `pid` has an entry under a /proc-style directory root.
pub(crate) fn proc_alive(proc_root: &Path, pid: u32) -> bool {
    proc_root.join(pid.to_string()).is_dir()
}

/// Read-only everything: a sealed golden cannot be written by anyone in this
/// uid. (True immutability — chattr +i — needs a syscall dep; the file-mode
/// seal is the M1 bar: accidental writes, not adversary root. ponytail:
/// revisit with `nix`/`libc` if fleet ever runs multi-user.)
#[cfg(target_os = "linux")]
fn seal_immutable(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o555))?;
                stack.push(p);
            } else {
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444))?;
            }
        }
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_cmd(argv: &[String]) -> anyhow::Result<()> {
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()?;
    if out.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "{}: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"kernel = "kata-kernel"
initramfs = "initrd2"
base_rootfs = "rootfs.ext4"
packages = ["vim-tiny"]
scripts = ["setup.sh"]
agents = ["vsockd"]
"#;

    fn w(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn fixture(dir: &Path) -> PathBuf {
        // fake inputs (hash reads them, so they must exist)
        w(dir, "kata-kernel", b"kernel-bytes");
        w(dir, "initrd2", b"initrd-bytes");
        w(dir, "rootfs.ext4", b"rootfs-bytes");
        w(dir, "setup.sh", b"touch /marker\n");
        w(dir, "vsockd", b"elf-bytes");
        let mp = dir.join("golden.toml");
        std::fs::write(&mp, SAMPLE).unwrap();
        mp
    }

    #[test]
    fn manifest_parse_and_defaults() {
        let dir = tempdir("parse");
        let mp = fixture(&dir);
        let m = Manifest::load(&mp).unwrap();
        // relative inputs resolve against the manifest dir (all platforms:
        // compare against Path::join results, never literal separators)
        assert_eq!(m.kernel, dir.join("kata-kernel"));
        assert_eq!(m.initramfs, dir.join("initrd2"));
        assert_eq!(m.base_rootfs, dir.join("rootfs.ext4"));
        assert_eq!(m.packages, ["vim-tiny"]);
        assert_eq!(m.agents, [dir.join("vsockd")]);
        assert_eq!(m.scripts, [dir.join("setup.sh")]);
        // truly-absolute path (platform-wise) passes through untouched
        let abs = dir.join("abs-kernel");
        std::fs::write(&abs, b"k").unwrap();
        std::fs::write(
            dir.join("abs.toml"),
            format!(
                "kernel = '{}'\ninitramfs = '{}'\nbase_rootfs = '{}'\n",
                abs.display(), abs.display(), abs.display()
            ),
        )
        .unwrap();
        let m = Manifest::load(&dir.join("abs.toml")).unwrap();
        assert_eq!(m.kernel, abs);
        let minimal = r#"kernel = "k"
initramfs = "i"
base_rootfs = "r"
"#;
        std::fs::write(dir.join("min.toml"), minimal).unwrap();
        let m = Manifest::load(&dir.join("min.toml")).unwrap();
        assert!(m.packages.is_empty() && m.scripts.is_empty() && m.agents.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn manifest_rejects_unknown_key() {
        // a typo'd input must not silently bake the wrong golden
        let dir = tempdir("unknown");
        std::fs::write(dir.join("m.toml"), "kernel = \"/k\"\ninitramfs = \"/i\"\nbase_rootfs = \"/r\"\npacakges = []\n").unwrap();
        let e = Manifest::load(&dir.join("m.toml")).unwrap_err();
        assert!(e.to_string().contains("acakges"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hash_deterministic_and_input_sensitive() {
        let dir = tempdir("hash");
        let mp = fixture(&dir);
        let m = Manifest::load(&mp).unwrap();
        let bytes = std::fs::read(&mp).unwrap();
        let h1 = m.content_hash(&bytes).unwrap();
        let h2 = m.content_hash(&bytes).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
        // changing a referenced file's content changes the hash
        w(&dir, "setup.sh", b"echo different\n");
        let h3 = m.content_hash(&bytes).unwrap();
        assert_ne!(h1, h3, "script content change must change hash");
        // changing manifest bytes (a comment) changes the hash
        let mut commented = bytes.to_vec();
        commented.extend_from_slice(b"\n# note\n");
        assert_ne!(m.content_hash(&commented).unwrap(), h3);
        // missing input file is an error, not a silent hash
        std::fs::remove_file(dir.join("vsockd")).unwrap();
        assert!(m.content_hash(&bytes).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hash_rejects_unreadable_input() {
        let dir = tempdir("miss");
        std::fs::write(dir.join("m.toml"), "kernel = \"/nope\"\ninitramfs = \"/i\"\nbase_rootfs = \"/r\"\n").unwrap();
        let m = Manifest::load(&dir.join("m.toml")).unwrap();
        let e = m.content_hash(b"x").unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn golden_hit_requires_no_marker() {
        let dir = tempdir("hit");
        let g = golden_dir(&dir, "abc");
        assert!(!golden_exists(&dir, "abc"));
        std::fs::create_dir_all(&g).unwrap();
        assert!(golden_exists(&dir, "abc"));
        std::fs::write(marker(&dir, "abc"), b"").unwrap();
        assert!(!golden_exists(&dir, "abc"), "crashed bake must not count as a hit");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn argv_builders_exact() {
        let mnt = Path::new("/g/mnt");
        assert_eq!(
            mount_loop_argv(Path::new("/g/rootfs.raw"), mnt),
            ["mount", "-o", "loop", "/g/rootfs.raw", "/g/mnt"]
        );
        // joined children compare against Path::join output (sep-agnostic)
        let proc = mnt.join("proc");
        let expected: Vec<String> =
            vec!["mount".into(), "--bind".into(), "/proc".into(), proc.display().to_string()];
        assert_eq!(bind_argv("/proc", &proc), expected);
        assert_eq!(umount_argv(mnt), ["umount", "/g/mnt"]);
        assert_eq!(umount_lazy_argv(mnt), ["umount", "-l", "/g/mnt"]);
        assert_eq!(
            apt_install_argv(mnt, &["vim-tiny".into(), "git".into()]),
            ["chroot", "/g/mnt", "apt-get", "-y", "-q", "install", "--no-install-recommends", "vim-tiny", "git"]
        );
        assert_eq!(script_argv(mnt, "/tmp/bay-setup-0.sh"), ["chroot", "/g/mnt", "bash", "/tmp/bay-setup-0.sh"]);
        assert_eq!(
            convert_argv(Path::new("/g/rootfs.raw"), Path::new("/g/disk.qcow2")),
            ["qemu-img", "convert", "-f", "raw", "-O", "qcow2", "/g/rootfs.raw", "/g/disk.qcow2"]
        );
    }

    #[test]
    fn mounts_under_deepest_first_and_prefix_scoped() {
        // fake mountinfo: loop mount at gdir/mnt, binds below it, a mount
        // under a look-alike prefix that must NOT match, foreign mounts.
        let info = "36 24 7:0 / /g/abc/mnt rw,relatime shared:1 - ext4 /dev/loop0 rw\n\
                    40 36 0:38 / /g/abc/mnt/dev rw shared:2 - tmpfs tmpfs rw\n\
                    41 36 0:39 / /g/abc/mnt/proc rw shared:3 - proc proc rw\n\
                    42 36 0:40 / /g/abc/mnt/sys rw shared:4 - sysfs sysfs rw\n\
                    43 24 8:1 / /g/abcd/mnt rw shared:5 - ext4 /dev/loop1 rw\n\
                    44 24 0:41 / /home/elsewhere rw shared:6 - tmpfs t rw\n";
        // children before parent; /g/abcd is a prefix-string match but not
        // a path-prefix match → excluded; order among equal-depth children
        // follows input.
        assert_eq!(
            mounts_under(info, "/g/abc"),
            ["/g/abc/mnt/dev", "/g/abc/mnt/proc", "/g/abc/mnt/sys", "/g/abc/mnt"]
        );
        // the mount point itself counts (crashes can mount exactly at gdir)
        let info2 = "36 24 7:0 / /g/abc rw,relatime shared:1 - ext4 /dev/loop0 rw\n";
        assert_eq!(mounts_under(info2, "/g/abc"), ["/g/abc"]);
        // malformed/short lines are ignored, not panics
        assert!(mounts_under("junk\n36 24\n", "/g").is_empty());
    }

    #[test]
    fn mountinfo_octal_unescape() {
        let info = "36 24 7:0 / /mnt/my\\040dir/sub rw shared:1 - ext4 /dev/loop0 rw\n";
        assert_eq!(mounts_under(info, "/mnt/my dir"), vec!["/mnt/my dir/sub".to_string()]);
        // non-octal backslash garbage passes through as-is
        let info2 = "36 24 7:0 / /weird\\999path rw shared:1 - ext4 /dev/loop0 rw\n";
        assert_eq!(mounts_under(info2, r"/weird\999path"), [r"/weird\999path"]);
    }

    #[test]
    fn loop_backing_scoped_and_deduped() {
        let info = "36 24 7:0 / /g/abc/mnt rw shared:1 - ext4 /dev/loop0 rw\n\
                    40 36 0:38 / /g/abc/mnt/dev rw shared:2 - tmpfs tmpfs rw\n\
                    43 24 8:1 / /g/zzz/mnt rw shared:5 - ext4 /dev/loop7 rw\n\
                    44 36 7:0 / /g/abc/mnt/extra rw shared:6 - ext4 /dev/loop0 rw\n";
        // only loop-backed mounts under the prefix, deduped (tmpfs source
        // is not a loop device; loop7 is another golden's)
        assert_eq!(loop_backing(info, "/g/abc"), vec!["/dev/loop0".to_string()]);
        assert!(loop_backing("not mountinfo at all", "/g").is_empty());
    }

    #[test]
    fn losetup_j_output_parsed() {
        let out = "/dev/loop3: [0807]:1234 (/g/abc/rootfs.raw)\n/dev/loop4: [0807]:1235 (/other)\n";
        assert_eq!(losetup_devices(out), ["/dev/loop3", "/dev/loop4"]);
        assert!(losetup_devices("").is_empty());
        assert!(losetup_devices("/dev/sda1: [66305]:2 (/x)\n").is_empty());
    }

    #[test]
    fn bake_socket_pid_own_scheme_only() {
        assert_eq!(bake_socket_pid("bay-bake-42.sock"), Some(42));
        assert_eq!(bake_socket_pid("bay-bake-42-vsock.sock"), Some(42));
        // near-misses must NOT match: other names, malformed pids, other
        // suffixes — the sweep must never claim a foreign /tmp file.
        for n in [
            "bay-baked.sock", "bay-bake-.sock", "bay-bake-x.sock",
            "bay-bake-42-tmp.sock", "bay-bake-42", "other-42.sock",
            "bay-bake-42.sock.bak", "bay-bake--42.sock",
        ] {
            assert_eq!(bake_socket_pid(n), None, "{n} must not match our scheme");
        }
    }

    #[test]
    fn proc_alive_reads_pid_dirs() {
        let dir = tempdir("proc");
        let proc = dir.join("fake-proc");
        std::fs::create_dir_all(proc.join("12345")).unwrap();
        assert!(proc_alive(&proc, 12345));
        assert!(!proc_alive(&proc, 12346));
        assert!(!proc_alive(&proc.join("nonexistent"), 12345));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("bay-bk-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

/// Exit test for #15: manifest → golden dir (kernel+initramfs+disk+snapshot),
/// second bake is a hash hit that touches nothing, the golden boots through
/// the real engine and its setup-script marker reads back over PTY, and the
/// sealed golden rejects writes. Env-gated + `#[ignore]`d so CI never boots
/// VMs; run on WSL2 (VM seat!) with the #16 kit env:
///
/// ```text
/// BAY_TEST_CH=/root/bay-spike/bin/ch-v50 BAY_TEST_REMOTE=/root/bay-spike/bin/ch-remote \
/// BAY_TEST_KERNEL=/root/bay-spike/img/kata-kernel BAY_TEST_INITRD=/root/bay-spike/img/initrd2 \
/// BAY_TEST_ROOTFS=/root/bay-spike/img/rootfs.ext4 cargo test -- --ignored
/// ```
#[cfg(all(unix, test))]
mod integration {
    use super::*;
    use crate::engine::{Engine, Golden};
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "boots real VMs; needs WSL2 + BAY_TEST_{CH,REMOTE,KERNEL,INITRD,ROOTFS}"]
    fn bake_idempotent_and_bootable() -> anyhow::Result<()> {
        std::env::set_var("BAY_CH_BIN", crate::ch50::integration::kit("BAY_TEST_CH"));
        std::env::set_var("BAY_CH_REMOTE", crate::ch50::integration::kit("BAY_TEST_REMOTE"));
        // short path: AF_UNIX 108-byte budget for the bake api socket
        let root = std::path::Path::new("/tmp").join(format!("bay-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(&home)?;
        std::env::set_var("BAY_HOME", &home);

        // manifest: kit kernel/initrd/rootfs, no apt packages (the apt argv
        // path is unit-tested; chroot execution is exercised via the script),
        // one setup script that stamps a marker into the rootfs.
        let marker = format!("BAKEMARKER-{}", std::process::id());
        std::fs::write(
            root.join("setup.sh"),
            format!("echo {marker} > /bay-bake-marker\n"),
        )?;
        let mp = root.join("golden.toml");
        std::fs::write(
            &mp,
            format!(
                "kernel = {:?}\ninitramfs = {:?}\nbase_rootfs = {:?}\nscripts = [\"setup.sh\"]\n",
                crate::ch50::integration::kit("BAY_TEST_KERNEL"),
                crate::ch50::integration::kit("BAY_TEST_INITRD"),
                crate::ch50::integration::kit("BAY_TEST_ROOTFS"),
            ),
        )?;

        // 1. bake once — fresh
        let t0 = Instant::now();
        let first = run(&mp)?;
        let dur = t0.elapsed();
        assert!(first.baked, "first bake must be fresh");
        assert_eq!(first.hash.len(), 64);
        let g = &first.dir;
        for f in ["kernel", "initramfs", "disk.qcow2", "overlay.tmpl.qcow2",
                  "manifest.toml", "serial.log",
                  "snapshot/config.json", "snapshot/state.json", "snapshot/memory-ranges"] {
            assert!(g.join(f).exists(), "golden missing {f}");
        }
        assert!(String::from_utf8_lossy(&std::fs::read(g.join("serial.log"))?).contains("BAY_READY"));
        assert!(!g.join("rootfs.raw").exists() && !g.join("mnt").exists(), "staging left in golden");
        assert!(!super::marker(&home, &first.hash).exists(), "stale in-flight marker");
        println!("BAKE-DURATION: {dur:?} hash={}", first.hash);

        // 2. sealed: files 0444, dirs 0555 (the seal seal_immutable applies;
        // asserting modes, not EPERM, because this test runs as root, which
        // DAC does not stop — true adversarial immutability would need
        // chattr +i, see seal_immutable's ponytail note).
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(g)?.permissions().mode() & 0o777, 0o555);
        assert_eq!(std::fs::metadata(g.join("disk.qcow2"))?.permissions().mode() & 0o777, 0o444);
        assert_eq!(std::fs::metadata(g.join("snapshot"))?.permissions().mode() & 0o777, 0o555);

        // 3. re-bake: hash hit, no-op
        let snap_mtime = std::fs::metadata(g.join("snapshot/config.json"))?.modified()?;
        let second = run(&mp)?;
        assert!(!second.baked, "re-bake must be a hash hit");
        assert_eq!(second.hash, first.hash);
        assert_eq!(std::fs::metadata(g.join("snapshot/config.json"))?.modified()?, snap_mtime, "hash hit touched files");

        // 4. boot the golden through the real engine; the chroot'd script's
        //    marker must read back over the vsock PTY (snapshot restores a
        //    live guest agent, the disk carries the baked state).
        let mut eng = crate::ch50::Ch50Engine::new(root.join("bays"))?;
        let p = eng.boot(1, &Golden { path: g.clone() })?;
        let out = crate::ch50::integration::pty(&p.socket, "cat /bay-bake-marker; exit\n")?;
        assert!(out.contains(&marker), "marker not in booted golden: {out:?}");
        eng.kill(1)?;
        drop(eng);

        // 5. no orphans (comm-name match; pgrep -f would hit this test's own
        // shell — spike gotcha #2).
        let out = std::process::Command::new("pgrep")
            .args(["-x", "ch-v50"])
            .output()?;
        anyhow::ensure!(out.stdout.is_empty(), "orphaned CH processes: {}", String::from_utf8_lossy(&out.stdout));

        // cleanup: as root the sealed 0555 dirs are still removable
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// #28 round-1 blocker, live: SIGKILL a `bay bake` child while its
    /// gdir/mnt loop mount is up (sleeping setup script keeps the chroot
    /// window open), then run the SAME bake again — the recovery teardown
    /// must clear the EBUSY wedge and seal. Needs WSL2 root + kit, and a
    /// built CLI (`cargo build` first; override path with BAY_BIN).
    #[test]
    #[ignore = "kills processes and mounts loops; needs WSL2 root + #16 kit"]
    fn sigkill_in_mount_window_recovers() -> anyhow::Result<()> {
        std::env::set_var("BAY_CH_BIN", crate::ch50::integration::kit("BAY_TEST_CH"));
        std::env::set_var("BAY_CH_REMOTE", crate::ch50::integration::kit("BAY_TEST_REMOTE"));
        let root = std::path::Path::new("/tmp").join(format!("bay-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(&home)?;
        std::env::set_var("BAY_HOME", &home);

        // setup script sleeps: bake sits inside the mount-to-flip window.
        std::fs::write(root.join("setup.sh"), "sleep 300\n")?;
        let mp = root.join("golden.toml");
        std::fs::write(
            &mp,
            format!(
                "kernel = {:?}\ninitramfs = {:?}\nbase_rootfs = {:?}\nscripts = [\"setup.sh\"]\n",
                crate::ch50::integration::kit("BAY_TEST_KERNEL"),
                crate::ch50::integration::kit("BAY_TEST_INITRD"),
                crate::ch50::integration::kit("BAY_TEST_ROOTFS"),
            ),
        )?;
        let m = Manifest::load(&mp)?;
        let hash = m.content_hash(&std::fs::read(&mp)?)?;
        let gdir = golden_dir(&home, &hash);

        // the CLI binary the child bakes with: next to the test binary in
        // target/debug/, or explicit BAY_BIN.
        let bin = std::env::var_os("BAY_BIN").map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().and_then(|p| p.parent()).map(|d| d.join("bay")))
                .unwrap_or_else(|| PathBuf::from("bay"))
        });
        let mut child = std::process::Command::new(&bin)
            .arg("bake")
            .arg(&mp)
            .env("BAY_HOME", &home)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", bin.display()))?;

        // wait for the mount window: gdir/mnt visible in mountinfo
        let prefix = gdir.display().to_string();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
            if !mounts_under(&info, &prefix).is_empty() {
                break;
            }
            anyhow::ensure!(
                child.try_wait()?.is_none(),
                "bake child exited before the mount window opened"
            );
            anyhow::ensure!(Instant::now() < deadline, "mount window never opened");
            std::thread::sleep(Duration::from_millis(50));
        }
        child.kill()?; // SIGKILL mid-window: the in-memory stack dies with it
        let _ = child.wait()?;
        assert!(super::marker(&home, &hash).exists(), "crash must leave the in-flight marker");
        // plant dead-pid + foreign sockets: recovery sweeps ours, never the
        // foreign file.
        let tmp = std::env::temp_dir();
        let own = tmp.join("bay-bake-4294967295.sock"); // pid that cannot exist
        let foreign = tmp.join("bay-bake-impostor.sock");
        std::fs::write(&own, b"")?;
        std::fs::write(&foreign, b"")?;

        // the wedge: pre-fix this Errs EBUSY forever. Must now succeed.
        let out = run(&mp)?;
        assert!(out.baked, "recovery bake must be a real bake");
        assert_eq!(out.hash, hash);
        assert!(!super::marker(&home, &hash).exists());

        // no stranded mounts/loops under gdir, staging gone, sockets swept
        let info = std::fs::read_to_string("/proc/self/mountinfo")?;
        assert!(mounts_under(&info, &prefix).is_empty(), "stranded mounts remain");
        assert!(loop_backing(&info, &prefix).is_empty(), "stranded loop devices remain");
        assert!(!gdir.join("mnt").exists() && !gdir.join("rootfs.raw").exists());
        assert!(!own.exists(), "dead-pid bake socket not swept");
        assert!(foreign.exists(), "sweep must not touch foreign /tmp files");
        let _ = std::fs::remove_file(&foreign);

        // kill any VM the dead child may have left (comm name only, spike
        // gotcha #2) and the scratch tree.
        let _ = std::process::Command::new("killall").arg("ch-v50").output();
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}
