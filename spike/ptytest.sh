#!/usr/bin/env bash
# bay M0 spike: vsock PTY round-trip test on Cloud Hypervisor (WSL2).
# usage: ptytest.sh <ch-binary> [ram]
cd /root/bay-spike # fixed layout: bin/ img/ kit/ under WSL ext4
BIN=${1:-./bin/ch-v50}
RAM=${2:-64M}
pkill -9 -f "bin/ch-v" 2>/dev/null || true
rm -f api.sock vsock.sock t.log ptyout.txt final1.txt
sleep 0.5
timeout 40 "$BIN" --api-socket api.sock --kernel img/kata-kernel --initramfs img/initrd2 \
  --disk path=img/rootfs.ext4 --memory size=$RAM --cpus boot=1 --console off --serial tty \
  --vsock cid=3,socket=vsock.sock --cmdline "console=ttyS0" > t.log 2>&1 &
CH=$!
BOOT=""
for i in $(seq 1 800); do grep -qa BAY_READY t.log 2>/dev/null && { BOOT=yes; break; }; sleep 0.05; done
if [ -z "$BOOT" ]; then echo NOBOOT > final1.txt; kill -9 $CH 2>/dev/null; exit 0; fi
( printf 'CONNECT 7\n'; sleep 1; printf 'echo PINGTEST_OK\n'; sleep 2 ) | timeout 8 nc -U vsock.sock > ptyout.txt 2>&1
{
  echo "BYTES=$(wc -c < ptyout.txt)"
  grep -a PINGTEST ptyout.txt | head -2
  grep -aoE "VSOCKD-LISTENING|VSOCKD-ACCEPT|forkpty.*" t.log | tail -3
  grep -aq PTS_FAIL t.log && echo PTS-PROBLEM || echo PTS-OK
  grep -aqE "BufDescMissing|queue error" t.log && echo CH-RX-ERRORS || echo CH-RX-CLEAN
} > final1.txt 2>&1
kill -9 $CH 2>/dev/null
rm -f api.sock vsock.sock
cat final1.txt
