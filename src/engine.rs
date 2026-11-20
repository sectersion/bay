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
    fn boot(&mut self, golden: &Golden) -> anyhow::Result<Pty>;

    /// Snapshot + stop. Parked bays must cost ~zero RAM.
    fn halt(&mut self, bay: u32) -> anyhow::Result<()>;

    /// Wake in place from the last halt.
    fn resume(&mut self, bay: u32) -> anyhow::Result<Pty>;

    /// Destroy the bay and its CoW disk. Idempotent.
    fn kill(&mut self, bay: u32) -> anyhow::Result<()>;
}

/// Pick the engine for the current host.
pub fn native_engine() -> anyhow::Result<Box<dyn Engine>> {
    anyhow::bail!(
        "no engine implemented yet; target for M1: Cloud Hypervisor on Linux (WSL2 counts). See ROADMAP.md."
    )
}
