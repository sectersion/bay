# bay architecture

Status: v1 design. Milestones live as GitHub issues (#1–#5), not files.

## Problem

Agent fleets run on git worktrees today. Worktrees isolate files and share
everything else: one `~/.claude` and MCP credentials, one Docker daemon, one
localhost:3000, one `.git` with lock fights, and one machine where any agent
with root owns everyone.

## Product

`bay` gives each agent a disposable microVM home on your own machine. Bays are
CoW clones of a golden snapshot, boot in ~200ms, share nothing with each other
or the host unless explicitly mounted, and can be parked (snapshot → zero RAM)
and woken in place. The snapshot layer is the undo button: revert a rogue
agent to its exact pre-crash state.

```
bay bake <manifest>          # golden snapshot: tools, dotfiles, agent CLIs
bay up 4 --repo .            # N bays, CoW clones of the golden
bay exec 2 -- claude "..."   # PTY pass-through into a bay
bay halt 3 / bay wake 3      # park / resume
bay kill 3 / bay kill --all  # destroy
```

## System

One binary, two faces:

- **CLI** — parses, talks to the daemon over a local socket, streams the PTY.
- **daemon** (`bay daemon`, one per user) — owns fleet state, the golden
  snapshot, and child VM processes. Everything the CLI can do, anything else
  can do against the same socket. GUI/TUI is a client, never an owner.

The daemon is organized around exactly one seam per hard problem:

```
              ┌──────────────┐
 CLI ──socket─│  fleet core  │   leases, state layout, park/wake bookkeeping
              └──────┬───────┘
                     │ Engine trait          ┌ virtio-9p (p9fs): repo + shares
              ┌──────┴───────┐               │ vsock: PTY + egress proxy
              │   engines    │───────────────┤
              └──────────────┘               └ (guest agent: envd-lite inside)
```

### Engines

`Engine` (`src/engine.rs`) is the whole hypervisor abstraction:
`boot(golden) → bay`, `pause`, `resume`, `kill`. No fleet logic may call a
hypervisor API directly; CI runs all three platforms from day one.

| host | engine | notes |
|---|---|---|
| Linux | **Cloud Hypervisor** (subprocess, API socket) | full power: snapshot/restore, qcow2 CoW, vsock. Don't embed; drive. |
| macOS | **Apple Virtualization.framework** | boot + checkpoint/restore (macOS 14+), virtio-vsock host API, CoW via APFS `clonefile`. Thin Swift shim or objc2 bindings. |
| Windows | **WSL2 running the Linux engine** | not a port; the CH engine, inside WSL2, with the full feature set including snapshots. Native WHPX is a possible later engine, not a v1 dependency. |

### Common services (engine-independent)

- **File sharing: one 9p server (p9fs, pure Rust)** in the daemon, presented
  to guests as virtio-9p. One code path on all three hosts. Repo clones and
  opt-in shares go through it. (virtio-fs on Linux only — deliberately not:
  one mechanism everywhere beats the faster-but-forked one.)
- **Networking: deny-by-default via vsock→SOCKS egress proxy.** Guests get no
  TAP/WinTap/CoreDevice — every byte leaves through one vsock port the daemon
  proxies with an allowlist. Sidesteps per-OS networking entirely, and the
  allowlist *is* the "none of them ever reach each other or your host"
  feature.
- **PTY: virtio-vsock, one listener per bay.** The CLI pass-through is a
  plain terminal-over-socket relay.
- **Guest agent**: one static binary inside the golden image (login/PTY spawn,
  mounts, git clone-from-local). The only guest-side code; keep it boring.

### State layout

```
~/.bay/
  golden/<hash>/          kernel, rootfs, memory snapshot, manifest
  bays/<id>/              disk CoW overlay, live state, snapshots
  bayd.sock
```

`bay bake` writes a new golden; bays pin the golden hash they were born from,
so re-baking never mutates a running or parked bay.

## Security posture

- Bays reach nothing: not the host, not other bays. Shares are opt-in,
  read-only by default.
- Egress is per-bay allowlists through the single SOCKS chokepoint.
- Per-bay credentials (clone `~/.claude` into the golden, tokens minted per
  `up`) — blast radius of a pwned agent is one CoW disk.

## Non-goals

Kubernetes runtime · cloud multi-tenancy · nested virt · GUI (until the CLI
bores us) · native Windows WHPX engine in v1 · running anything that isn't a
one-shot-to-fleet compute pattern (this is a home for agents, not OrbStack).

## Open questions

1. **Manifest format** — TOML with base image + package list + setup scripts
   + agent CLIs is the lean guess. Bake is slow path (once), so the format can
   stay dumb.
2. **Golden image build** — kernel + initramfs + rootfs: from distro
   packages (apt/dnf) vs `debootstrap`-style vs ext4 from Dockerfile layers.
   M1 decides this by doing it once.
3. **Arch support** — x86_64 first; aarch64 (ARM Macs) via CH is supported
   but RAM/CoW semantics differ; bake produces per-arch goldens.
