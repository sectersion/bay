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
    // exist half-written).
    if gdir.exists() {
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
    // stack of mounted paths in mount order; teardown always in reverse,
    // best-effort on the failure path so a mid-chroot error never strands a
    // loop device (a stranded mount once survived a failed bake at
    // /tmp/bay-bk-*, holding a deleted rootfs.raw open).
    let mut stack = vec![mnt.clone()];
    let flip = |stack: &mut Vec<PathBuf>, eager: bool| -> anyhow::Result<()> {
        while let Some(p) = stack.pop() {
            if eager {
                run_cmd(&umount_argv(&p))?;
            } else {
                let _ = run_cmd(&umount_argv(&p));
            }
        }
        Ok(())
    };
    let staged = (|| -> anyhow::Result<()> {
        run_cmd(&mount_loop_argv(&raw, &mnt))?;
        for src in ["/dev", "/proc", "/sys"] {
            let dst = mnt.join(src.trim_start_matches('/'));
            run_cmd(&bind_argv(src, &dst))?;
            stack.push(dst);
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
        let _ = flip(&mut stack, false);
        let _ = std::fs::remove_dir_all(&mnt);
        return Err(e);
    }
    flip(&mut stack, true)?;
    std::fs::remove_dir_all(&mnt)?;
    run_cmd(&convert_argv(&raw, &disk))?;
    std::fs::remove_file(&raw)?;
    Ok(())
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
    use std::time::Instant;

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
}
