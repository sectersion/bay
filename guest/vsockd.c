/*
 * bay guest agent (M1d, #17): one vsock connection = one exec.
 *
 * Wire (documented here, consumed by src/exec.rs):
 *   First line from host:  "<nonce> <shell command>\n"
 *     nonce = token without spaces, echoed back in the exit sentinel;
 *     shell command = the rest of the line, run as `bash --login -c <cmd>`.
 *     argv is joined + shell-quoted host-side, so a newline or the end of
 *     the line terminates the command (commands containing raw newlines are
 *     out of scope for M1 — pass `printf 'a\nb'` instead).
 *   Stream after that: raw PTY output until the command exits, then the
 *   agent's own final line: "BAY_EXIT <nonce> <code>\n" (shell convention:
 *   signal death = 128+signo). The sentinel is always the last thing on the
 *   socket before close; the host strips it and exits with <code>.
 *
 * Host connects to CH's per-bay vsock unix socket and issues "CONNECT 7\n"
 * (M0 finding, spike #1) before speaking this wire.
 *
 * Build (static, so guest glibc is irrelevant) and install into the golden:
 *   gcc -O2 -static -o vsockd guest/vsockd.c -lutil
 *   manifest: agents = ["<path>/vsockd"]   (lands in /usr/local/bin/)
 *   script:   ln -sf /usr/local/bin/vsockd /usr/local/sbin/vsockd
 *   (the kit rootfs /init starts /usr/local/sbin/vsockd before BAY_READY,
 *   so the snapshot freezes a live listener on port 7.)
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <string.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <pty.h>
#include <linux/vm_sockets.h>

/* relay one connection until the command exits; never returns */
static void serve(int c) {
    /* main() self-reaps relay children via SIG_IGN, but this process must
       waitpid its own grandchild: restore default so waitpid works */
    signal(SIGCHLD, SIG_DFL);
    char line[8192];
    size_t len = 0;
    /* read first line */
    while (len < sizeof line - 1) {
        ssize_t n = read(c, line + len, 1);
        if (n <= 0 || line[len] == '\n') break;
        len++;
    }
    line[len] = 0;
    char *sp = strchr(line, ' ');
    if (!sp || len == 0) _exit(1);
    *sp = 0;
    char *nonce = line, *cmd = sp + 1;
    if (*cmd == 0) _exit(1);

    signal(SIGPIPE, SIG_IGN); /* host may hang up first */
    int m;
    pid_t p = forkpty(&m, NULL, NULL, NULL);
    if (p < 0) _exit(1);
    if (p == 0) {
        setenv("TERM", "xterm-256color", 1);
        execlp("bash", "bash", "--login", "-c", cmd, (char *)0);
        _exit(127);
    }
    fd_set r;
    char b[4096];
    for (;;) {
        FD_ZERO(&r);
        FD_SET(c, &r);
        FD_SET(m, &r);
        int x = c > m ? c : m;
        if (select(x + 1, &r, NULL, NULL, NULL) <= 0) break;
        if (FD_ISSET(c, &r)) {
            ssize_t n = read(c, b, sizeof b);
            if (n <= 0) { kill(p, SIGHUP); break; } /* host hung up */
            if (write(m, b, n) <= 0) break;
        }
        if (FD_ISSET(m, &r)) {
            ssize_t n = read(m, b, sizeof b);
            if (n <= 0) break; /* command exited */
            if (write(c, b, n) <= 0) { kill(p, SIGHUP); break; }
        }
    }
    int st = 0;
    waitpid(p, &st, 0);
    int code = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
    char out[256];
    int n = snprintf(out, sizeof out, "\nBAY_EXIT %s %d\n", nonce, code);
    if (n > 0) { ssize_t _w = write(c, out, n); (void)_w; }
    _exit(0);
}

int main(void) {
    signal(SIGCHLD, SIG_IGN); /* relay children self-reap */
    int ls = socket(AF_VSOCK, SOCK_STREAM, 0), one = 1;
    if (ls < 0) { perror("socket"); return 1; }
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_vm sa;
    memset(&sa, 0, sizeof sa);
    sa.svm_family = AF_VSOCK;
    sa.svm_cid = VMADDR_CID_ANY;
    sa.svm_port = 7;
    if (bind(ls, (struct sockaddr *)&sa, sizeof sa) || listen(ls, 8)) { perror("listen"); return 1; }
    printf("VSOCKD-LISTENING\n");
    for (;;) {
        int c = accept(ls, NULL, NULL);
        if (c < 0) continue;
        pid_t p = fork();
        if (p == 0) { serve(c); }
        close(c);
    }
}
