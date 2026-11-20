# roadmap

Each milestone ends with something you can run. Dates are anti-dates: if a
milestone takes longer than the next one after it, cut scope.

## M0 — the spike (de-risk everything)

Prove the load-bearing claim: a 50MB-RAM bay boots from a Cloud Hypervisor
snapshot in ~200ms with a vsock shell, inside WSL2.

- [ ] Build a minimal golden: kernel + busybox initramfs + one ext4 rootfs
- [ ] Boot it with CH, `--restore` it, measure cold-boot vs restore latency
- [ ] Get a shell over vsock (`nc` to the unix socket is fine for the spike)
- [ ] **Go/no-go:** <300ms + working PTY ⇒ build M1. Meh ⇒ try QEMU as the
      Linux engine before rethinking the product.

## M1 — Linux engine, one bay end-to-end

- [ ] `bay daemon` + CLI plumbing; `bake/up/exec/kill` against one CH process
- [ ] Manifest → golden bake (single distro, single arch)
- [ ] qcow2 CoW overlay per bay; `git clone file://` of a local repo
- [ ] p9fs share path; vsock PTY relay in the CLI
- [ ] Exit test: two bays of the same repo, both run an agent command, neither
      can see the other (`ls /mnt`, ports, `ps`).

## M2 — park (the undo button)

- [ ] `halt`/`wake`: memory snapshot per bay, pinned golden hash
- [ ] `bay revert <id> --to <halt>` and per-turn halt hooks
- [ ] Egress allowlist enforced at the SOCKS proxy; default-deny host/peer
- [ ] Exit test: halt 10 bays, RAM goes to ~0, wake any one, its tmux session
      is still there.

## M3 — three-platform

- [ ] macOS engine (Virtualization.framework shim): boot, PTY, checkpoint
- [ ] Windows = WSL2 packaging: install script, `bay` on PATH reaches the
      in-WSL daemon
- [ ] CI: three runners, `M1 exit test` matrix
- [ ] Exit test: same README commands work on all three OSes.

## M4 — fleet ergonomics

- [ ] `--repo` auto-clone, named bays, `bay ls` with state/age/egress stats
- [ ] TUI dashboard (the "desktop app" that isn't a desktop app)
- [ ] `bay fork <id>` — same golden + same disk state, two futures
- [ ] Docs site, install script, first release tag.

## Later, not scoped

native WHPX engine · virtio-fs fast path on Linux · diff-snapshots (dedup
parked bays) · per-bay resource quotas · the actual GUI.
