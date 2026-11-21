# M0 spike — verdict inputs

Ran in WSL2 (Ubuntu, Windows 11, AMD host, `/dev/kvm` via nested virt).
Engine: **Cloud Hypervisor v50** — v55 refuses to boot here because WSL's
Microsoft KVM lacks `KVM_CAP_USER_MEMORY2`; v34 has a broken vsock RX ring
against modern guest drivers. v50 is the sweet spot on this platform for now.
Guest kernel: Kata Containers 4.4.0 `vmlinux-6.18.62-211` (ELF — v50 cannot
boot Debian's bzImage; vsock built-ins plus modules extracted from the Kata
ubuntu image). Rootfs: Debian bookworm minbase. PTY: static `vsockd`
(Debian's socat has no AF_VSOCK support) — the same shape M1's guest agent
will take.

## Results (`measure.sh <ram>`, repeated passes agree within ~100ms)

| metric | 64M | 50M |
|---|---|---|
| cold boot to BAY_READY | 1687ms | 1802ms |
| snapshot size | 65MB | 51MB |
| **restore → vsock socket live** | **77ms** | **36ms** |
| cold PTY round trip | OK | OK |
| restored PTY round trip | OK | OK |

`restore-pty=8090ms` in raw output is a harness artifact (fixed sleeps in the
probe); the real usable-state signal is restore→socket + resume + one
interactive echo, all clean.

## Verdict: **GO** — build M1 with CH v50 on Linux/WSL2.

## Known-good command shapes (v50)

```
ch-v50 --api-socket api.sock --kernel img/kata-kernel --initramfs img/initrd2 \
  --disk path=img/rootfs.ext4 --memory size=50M --cpus boot=1 --console off \
  --serial tty --vsock cid=3,socket=vsock.sock --cmdline "console=ttyS0"
ch-remote --api-socket api.sock pause
ch-remote --api-socket api.sock snapshot file://$PWD/snap     # dir must exist
ch-v50 --api-socket api2.sock --restore source_url=file://$PWD/snap
ch-remote --api-socket api2.sock resume      # restored VM comes up paused
# PTY: printf 'CONNECT 7\n' then bytes over nc -U vsock.sock (host→guest
# needs devpts mounted in guest or forkpty fails silently)
```

## WSL2 gotchas learned (recorded so M1 doesn't rediscover them)

1. Mounts, sockets, and VM processes die between `wsl` invocations (8s idle
   teardown) — one session per scenario, artifacts on ext4 not `/mnt/c`.
2. `pkill -f` patterns match your own `bash -c` — use `killall` on comm names.
3. CH binds `--api-socket` before parsing `--restore`; failed parses leave
   stale socket files → phantom EADDRINUSE. Unlink first.
4. v34 vs v50 CLI drift: `vm.pause`(v55)/`pause`(v34/50),
   `source_url=` restore key, `file://` URL destinations.

## Rebuild from zero

`bash build-rootfs.sh` (~10 min, network + disk needed), then
`bash measure.sh 50M`. Scripts are the ones that actually produced these
numbers; the WSL working copy lives at `/root/bay-spike` (deleted after this
verdict is recorded and the spike removed per issue #1).
