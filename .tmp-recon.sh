#!/bin/bash
set -x
which cargo || echo NO-CARGO
which git || echo NO-GIT
ls /root/bay-spike/bin /root/bay-spike/img 2>&1 | head -25
ls -d /root/bay* /root/fleet 2>/dev/null
df -T /root | tail -1
