#!/usr/bin/env bash
# bay M0 spike: build the boot kit (runs in WSL2 Ubuntu, as root, ext4 home — never /mnt/c).
# One-time: ~10 min (debootstrap + apt + Kata bundle download).
set -euo pipefail
S=${S:-/root/bay-spike}
BIN_V=50
mkdir -p $S/bin $S/img $S/kit
cd $S

# 1. Hypervisor + remote (v50: last CH line that boots on WSL2's KVM; v55 demands
#    KVM_CAP_USER_MEMORY2 which the Microsoft WSL kernel does not implement)
[ -x bin/ch-v$BIN_V ] || { curl -sfL -o bin/ch-v$BIN_V https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v$BIN_V.0/cloud-hypervisor-static && chmod +x bin/ch-v$BIN_V; }
[ -x bin/ch-remote ] || { curl -sfL -o bin/ch-remote https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v$BIN_V.0/ch-remote-static && chmod +x bin/ch-remote; }

# 2. Debian rootfs (user decision: Debian golden), minbase + kernel + socat + kmod
if [ ! -e img/rootfs.ext4 ]; then
  apt-get install -y -q debootstrap
  truncate -s 1600M img/rootfs.ext4 && mkfs.ext4 -q -F img/rootfs.ext4
  mkdir -p img/mnt && mount -o loop img/rootfs.ext4 img/mnt
  debootstrap --variant=minbase --include=ca-certificates bookworm img/mnt http://deb.debian.org/debian
  for a in proc sys dev; do mount -t $a $a img/mnt/$a 2>/dev/null || { [ $a = proc ] || mount --rbind /$a img/mnt/$a; mount -t proc proc img/mnt/proc; }; done
  cp -f /etc/resolv.conf img/mnt/etc/resolv.conf
  chroot img/mnt apt-get -y install --no-install-recommends linux-image-amd64 socat kmod || true # initramfs-tools postinst may fail in chroot; we use our own initrd
  cat > img/mnt/init <<'GINIT'
#!/bin/sh
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts -o mode=0620,gid=5,ptmxmode=0666 2>/dev/null || echo PTS_FAIL
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
/usr/local/sbin/vsockd &
sleep 1
kill -0 $! 2>/dev/null && echo VSOCKD-ALIVE || echo VSOCKD_DIED
echo BAY_READY
wait
GINIT
  chmod +x img/mnt/init
fi

# 3. vsockd: static PTY-bridge over AF_VSOCK (Debian socat has no VSOCK support;
#    built with host gcc — static, so guest glibc version is irrelevant)
if [ ! -x img/../kit/vsockd ] && [ ! -f img/mnt/usr/local/sbin/vsockd ]; then
  apt-get install -y -q gcc socat netcat-openbsd busybox-static cpio zstd >/dev/null
  cat > kit/vsockd.c <<'CFILE'
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <string.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/select.h>
#include <pty.h>
#include <linux/vm_sockets.h>
int main(void){
  signal(SIGCHLD,SIG_IGN);
  int ls=socket(AF_VSOCK,SOCK_STREAM,0), one=1;
  if (ls<0){ perror("socket"); return 1; }
  setsockopt(ls,SOL_SOCKET,SO_REUSEADDR,&one,sizeof one);
  struct sockaddr_vm sa; memset(&sa,0,sizeof sa);
  sa.svm_family=AF_VSOCK; sa.svm_cid=VMADDR_CID_ANY; sa.svm_port=7;
  if(bind(ls,(struct sockaddr*)&sa,sizeof sa)||listen(ls,8)){perror("listen");return 1;}
  printf("VSOCKD-LISTENING\n");
  for(;;){ int c=accept(ls,NULL,NULL); if(c<0)continue;
    printf("VSOCKD-ACCEPT\n");
    int m; pid_t p=forkpty(&m,NULL,NULL,NULL);
    if(p<0){ perror("forkpty"); close(c); continue; }
    if(p==0){ setenv("TERM","xterm-256color",1); execlp("bash","bash","--login",(char*)0); _exit(127); }
    fd_set r; char b[4096];
    for(;;){ FD_ZERO(&r); FD_SET(c,&r); FD_SET(m,&r); int x=c>m?c:m;
      if(select(x+1,&r,NULL,NULL,NULL)<=0)break;
      if(FD_ISSET(c,&r)){ ssize_t n=read(c,b,sizeof b); if(n<=0)break; if(write(m,b,n)<=0)break; }
      if(FD_ISSET(m,&r)){ ssize_t n=read(m,b,sizeof b); if(n<=0)break; if(write(c,b,n)<=0)break; }
    }
    close(c); close(m);
  }
}
CFILE
  mountpoint -q img/mnt || mount -o loop img/rootfs.ext4 img/mnt
  gcc -O2 -static -o kit/vsockd kit/vsockd.c -lutil && cp kit/vsockd img/mnt/usr/local/sbin/vsockd
fi

# 4. Kata guest kernel (ELF — CH <=50 cannot boot bzImage; drivers built-in)
if [ ! -e img/kata-kernel ]; then
  [ -e kata.tar.zst ] || curl -sfL -o kata.tar.zst https://github.com/kata-containers/kata-containers/releases/download/4.4.0/kata-static-4.4.0-amd64.tar.zst
  tar --zstd -xf kata.tar.zst ./opt/kata/share/kata-containers/vmlinux-6.18.62-211
  mv opt/kata/share/kata-containers/vmlinux-6.18.62-211 img/kata-kernel
fi

# 5. Minimal initramfs: busybox + vsock modules from the Kata ubuntu image (the
#    Kata kernel ships vsock core built-in but the virtio transport as module)
if [ ! -e img/initrd2 ]; then
  [ -d kit/vsockmods ] || { LOOP=$(losetup -fP --show opt/kata/share/kata-containers/kata-ubuntu-resolute.image); mkdir -p kmodmnt; mount -o ro ${LOOP}p1 kmodmnt; VER=$(ls kmodmnt/lib/modules | head -1); mkdir -p kit/vsockmods; for n in vsock virtio_vsock vmw_vsock_virtio_transport_common vmw_vsock_virtio_transport; do f=$(find kmodmnt/lib/modules/$VER -name "$n.ko*" | head -1); cp "$f" kit/vsockmods/$n.ko.gz; done; umount kmodmnt; losetup -d $LOOP; }
  rm -rf kit/rd; mkdir -p kit/rd/{bin,mod,proc,sys,dev,r}
  cp /bin/busybox kit/rd/bin/busybox
  (cd kit/rd/bin && ./busybox --list | grep -vx busybox | while read -r a; do ln -sf busybox "$a"; done)
  cp kit/vsockmods/* kit/rd/mod/
  cat > kit/rd/init <<'RINIT'
#!/bin/busybox sh
mount -t devtmpfs dev /dev; mount -t proc proc /proc; mount -t sysfs sys /sys
i=0
while [ ! -e /dev/vda ] && [ $i -lt 4 ]; do for m in /mod/*; do insmod $m 2>/dev/null; done; i=$((i+1)); done
[ -e /dev/vda ] || { echo INITRD_NO_ROOT; while true; do sleep 1; done; }
mount -t ext4 -o rw /dev/vda /r || { echo INITRD_MOUNT_FAIL; while true; do sleep 1; done; }
exec switch_root /r /init
RINIT
  chmod +x kit/rd/init
  (cd kit/rd && find . | cpio --create --format=newc 2>/dev/null | gzip -9 > $S/img/initrd2)
fi

mountpoint -q img/mnt && umount img/mnt
echo "KIT-READY: $(ls -la img/ | awk '{print $9}' | grep -v '^$' | tr '\n' ' ')"
