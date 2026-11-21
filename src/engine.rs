//! The one seam between bay and every hypervisor. See docs/ARCHITECTURE.md.
//!
//! Implementations (M1: cloud-hypervisor subprocess; M3: virtualization-framework
//! shim, WSL2-hosted Linux engine) may not be referenced by fleet logic except
//! through this trait.

use std::path::PathBuf;

/// What a bay is born from: a golden snapshot baked by `bay bake`.
pub struct Golden {
    pub path: PathBuf, // ~/.bay/golden/<hash>/
}

/// Handle to the bay's terminal: a host-side vsock socket to relay stdio over.
pub struct Pty {
    pub socket: PathBuf,
}

pub trait Engine {
    fn name(&self) -> &'static str;

    /// Restore a bay from the golden with a fresh CoW disk. Target: ~200ms.
    /// `bay` is the daemon's fleet id (the same one halt/resume/kill take);
    /// the engine keys its per-bay state by it.
    fn boot(&mut self, bay: u32, golden: &Golden) -> anyhow::Result<Pty>;

    /// Snapshot + stop. Parked bays must cost ~zero RAM.
    fn halt(&mut self, bay: u32) -> anyhow::Result<()>;

    /// Wake in place from the last halt.
    fn resume(&mut self, bay: u32) -> anyhow::Result<Pty>;

    /// Destroy the bay and its CoW disk. Idempotent.
    fn kill(&mut self, bay: u32) -> anyhow::Result<()>;
}

/// Pick the engine for the current host.
#[cfg(target_os = "linux")]
pub fn native_engine() -> anyhow::Result<Box<dyn Engine>> {
    use crate::ch50::Ch50Engine;
    Ok(Box::new(Ch50Engine::new(
        crate::daemon::bay_home()?.join("bays"),
    )?))
}

/// Pick the engine for the current host.
#[cfg(not(target_os = "linux"))]
pub fn native_engine() -> anyhow::Result<Box<dyn Engine>> {
    anyhow::bail!(
        "no engine for this host yet: macOS ships in M3 (#4); on Windows run \
         `bay daemon` under WSL2 — the Cloud Hypervisor v50 engine (ch50) lives there."
    )
}
