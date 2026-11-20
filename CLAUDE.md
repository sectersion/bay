# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

bay — disposable microVM homes for coding-agent fleets. Each agent gets its
own bay: a CoW clone of a golden snapshot that boots in ~200ms, shares nothing
with other bays or the host, and can be parked (snapshot → zero RAM) and woken
in place. Full design: `docs/ARCHITECTURE.md`. Plan and todos: GitHub issues
— milestones M0–M4 are #1–#5, backlog #6.

## Repo rules (owner-set, absolute)

1. **Every change gets an issue and its own PR.** Issues are the global todo
   list. Never write TODOs, checklists, or roadmap content into files — file
   an issue.
2. **Two independent agent reviews must pass before any merge.** Each posts a
   formal review comment whose body starts `APPROVED` (or `CHANGES_REQUESTED`
   with specifics) and names the head SHA it reviewed. Verdicts only count
   against the final head: any push invalidates both reviews — re-run both
   until a clean round matches the SHA actually merged. (GitHub rejects
   approving reviews from the author's own account, so this gate is
   procedural, not settings-based; the squash-only merge-method setting is
   repo-enforced.)
3. **Never push to main.** Branch → PR → squash-merge. Protection enforces
   this, admins included.

## Commands

```
cargo test                      # build gate (CI runs on ubuntu/macos/windows)
cargo test cli_owns_its_args    # single test
cargo run -- up 2               # CLI; everything bails until M1 lands
```

## Architecture you can't see from the file tree

- **One binary, two faces.** The CLI is a thin client over `bayd.sock`;
  `bay daemon` is the only thing that owns fleet state, the golden image, and
  child VM processes. GUI/TUI will be clients too — never a second owner.
- **`src/engine.rs`'s `Engine` trait is the only hypervisor seam.** Fleet
  logic must not call a hypervisor API directly. Backends: Cloud Hypervisor
  as a subprocess (Linux; WSL2 is the Windows path, not a port), Apple
  Virtualization.framework via a thin shim (macOS).
- **Engine-independent services live in the daemon**: 9p file sharing via
  `p9fs` (one code path on all hosts — virtio-fs deliberately not used), PTY
  and egress over virtio-vsock. Networking is deny-by-default: no TAP; all
  bytes leave through one vsock→SOCKS proxy that *is* the egress allowlist.
- **State**: `~/.bay/golden/<hash>/` is immutable once baked; bays pin the
  golden hash they were born from, so re-baking never mutates running or
  parked bays. CoW overlays and snapshots under `~/.bay/bays/<id>/`.
- **Current state of the code**: CLI skeleton (clap), `Engine` trait stub,
  `main` bails by design. Real work starts with the M0 spike (#1, go/no-go):
  shell scripts in `spike/`, zero Rust, deleted once the verdict is recorded
  on the issue.
