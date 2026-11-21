#!/usr/bin/env bash
# bay M0 spike: cold boot / snapshot / restore timings (CH v50 + Kata 6.18 kernel on WSL2).
# usage: measure.sh [ram]   (default 64M)
cd /root/bay-spike
RAM=${1:-64M}
BIN=./bin/ch-v50
CHARGS="--kernel img/kata-kernel --initramfs img/initrd2 --disk path=img/rootfs.ext4 --memory size=$RAM --cpus boot=1 --console off --serial tty --vsock cid=3,socket=vsock.sock --cmdline console=ttyS0"
rm -rf snap api*.sock vsock.sock serial*.log pty*.txt err.txt
mkdir -p snap

echo "=== COLD BOOT (RAM=$RAM)"
t0=$(date +%s%N)
timeout 60 $BIN --api-socket api.sock $CHARGS > serial1.log 2>&1 &
CH=$!
while ! grep -qa BAY_READY serial1.log 2>/dev/null; do
  NOW=$(date +%s%N)
  [ $((NOW-t0)) -lt 60000000000 ] || { echo TIMEOUT_BOOT; tail -6 serial1.log; kill -9 $CH 2>/dev/null; exit 1; }
  sleep 0.02
done
t1=$(date +%s%N); COLD=$(( (t1-t0)/1000000 ))
echo "cold boot to BAY_READY: ${COLD}ms"

( printf 'CONNECT 7\n'; sleep 1; printf 'echo PING_COLD\n'; sleep 2 ) | timeout 8 nc -U vsock.sock > ptycold.txt 2>&1
grep -qa PING_COLD ptycold.txt && echo "cold PTY: OK" || echo "cold PTY: FAIL"

echo "=== PAUSE + SNAPSHOT + KILL"
timeout 10 ./bin/ch-remote --api-socket api.sock vm.pause 2>err.txt || timeout 10 ./bin/ch-remote --api-socket api.sock pause 2>>err.txt || { echo PAUSE_FAIL; cat err.txt; kill -9 $CH; exit 1; }
timeout 10 ./bin/ch-remote --api-socket api.sock vm.snapshot destination=file://$PWD/snap 2>>err.txt || timeout 10 ./bin/ch-remote --api-socket api.sock snapshot file://$PWD/snap 2>>err.txt || { echo SNAPSHOT_FAIL; cat err.txt; kill -9 $CH; exit 1; }
echo "snapshot OK: $(du -sh snap 2>/dev/null | cut -f1)"
kill -9 $CH 2>/dev/null; wait $CH 2>/dev/null
rm -f api.sock vsock.sock; sleep 0.3

echo "=== RESTORE (RAM=$RAM)"
killall -9 ch-v50 2>/dev/null; rm -f api.sock api2.sock vsock.sock; sleep 0.5
t2=$(date +%s%N)
timeout 60 $BIN --api-socket api2.sock --restore source_url=file://$PWD/snap > serial2.log 2>&1 &
CH2=$!
ok=""
for i in $(seq 1 3000); do [ -S vsock.sock ] && { ok=yes; break; }; sleep 0.005; done
[ -n "$ok" ] || { echo RESTORE_FAIL; tail -8 serial2.log; kill -9 $CH2 2>/dev/null; exit 2; }
t3=$(date +%s%N); SOCK=$(( (t3-t2)/1000000 ))
echo "restore->vsock socket live: ${SOCK}ms"
timeout 10 ./bin/ch-remote --api-socket api2.sock vm.resume > /dev/null 2>&1 || timeout 10 ./bin/ch-remote --api-socket api2.sock resume > /dev/null 2>&1
echo "resumed"
( printf 'CONNECT 7\n'; sleep 1; printf 'echo PING_WARM\n'; sleep 2 ) | timeout 8 nc -U vsock.sock > ptywarm.txt 2>&1
grep -qa PING_WARM ptywarm.txt && echo "warm PTY: OK" || echo "warm PTY: FAIL"
t4=$(date +%s%N)
kill -9 $CH2 2>/dev/null
echo "=== VERDICT-INPUTS ram=$RAM cold=${COLD}ms restore-socket=${SOCK}ms restore-pty=$(((t4-t2)/1000000))ms"
