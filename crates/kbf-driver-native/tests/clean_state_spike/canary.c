/* The canary of the clean-state spike: one small program, many verbs, each printing
 * `CANARY <verb> ok|errno=N(NAME) [detail]`. Built with clang on the runner (the spike
 * needs kernel objects, mach ports and crash makers a shell cannot make).
 * -DNO_MACH drops the verbs that need <servers/bootstrap.h>; -DNO_NOTIFY drops notify. */
#include <errno.h>
#include <stdint.h>
#include <fcntl.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/msg.h>
#include <sys/sem.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <unistd.h>
#include <os/log.h>
#ifndef NO_MACH
#include <mach/mach.h>
#include <servers/bootstrap.h>
#endif
#ifndef NO_NOTIFY
#include <notify.h>
#endif

static int result(const char *verb, int rc, const char *detail) {
    if (rc == 0) {
        printf("CANARY %s ok %s\n", verb, detail ? detail : "");
    } else {
        printf("CANARY %s errno=%d(%s) %s\n", verb, errno, strerror(errno), detail ? detail : "");
    }
    fflush(stdout);
    return rc == 0 ? 0 : 1;
}

/* Stay alive for the given seconds (default 300) so a held object stays held. */
static void hold(int secs) {
    alarm(secs > 0 ? secs : 300);
    for (;;) pause();
}

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: canary verb args\n"); return 2; }
    const char *v = argv[1];
    const char *a2 = argc > 2 ? argv[2] : NULL;
    const char *a3 = argc > 3 ? argv[3] : NULL;
    char detail[128] = "";

    if (!strcmp(v, "shm-create") && a2) {
        int fd = shm_open(a2, O_CREAT | O_RDWR, 0600);
        return result(v, fd < 0 ? -1 : 0, NULL);
    }
    if (!strcmp(v, "shm-open") && a2) {
        int fd = shm_open(a2, O_RDWR, 0);
        return result(v, fd < 0 ? -1 : 0, NULL);
    }
    if (!strcmp(v, "shm-unlink") && a2) return result(v, shm_unlink(a2), NULL);
    if (!strcmp(v, "sem-create") && a2) {
        sem_t *s = sem_open(a2, O_CREAT, 0600, 1);
        return result(v, s == SEM_FAILED ? -1 : 0, NULL);
    }
    if (!strcmp(v, "sem-open") && a2) {
        sem_t *s = sem_open(a2, 0);
        return result(v, s == SEM_FAILED ? -1 : 0, NULL);
    }
    if (!strcmp(v, "sem-unlink") && a2) return result(v, sem_unlink(a2), NULL);
    if (!strcmp(v, "sysv-shm")) {
        int id = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
        snprintf(detail, sizeof detail, "id=%d", id);
        return result(v, id < 0 ? -1 : 0, detail);
    }
    if (!strcmp(v, "sysv-sem")) {
        int id = semget(IPC_PRIVATE, 1, IPC_CREAT | 0600);
        snprintf(detail, sizeof detail, "id=%d", id);
        return result(v, id < 0 ? -1 : 0, detail);
    }
    if (!strcmp(v, "sysv-msg")) {
        int id = msgget(IPC_PRIVATE, IPC_CREAT | 0600);
        snprintf(detail, sizeof detail, "id=%d", id);
        return result(v, id < 0 ? -1 : 0, detail);
    }
#ifndef NO_MACH
    if (!strcmp(v, "mach-register") && a2) {
        mach_port_t port = MACH_PORT_NULL;
        kern_return_t kr = mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &port);
        if (kr == KERN_SUCCESS) kr = mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND);
        if (kr == KERN_SUCCESS) {
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            kr = bootstrap_register(bootstrap_port, (char *)a2, port);
        }
        snprintf(detail, sizeof detail, "kr=%d(%s)", kr, mach_error_string(kr));
        int rc = kr == KERN_SUCCESS ? 0 : -1;
        if (rc == 0) { result(v, 0, detail); if (a3) hold(atoi(a3)); return 0; }
        errno = EPERM;
        return result(v, -1, detail);
    }
    if (!strcmp(v, "mach-lookup") && a2) {
        mach_port_t port = MACH_PORT_NULL;
        kern_return_t kr = bootstrap_look_up(bootstrap_port, (char *)a2, &port);
        snprintf(detail, sizeof detail, "kr=%d(%s)", kr, mach_error_string(kr));
        errno = ENOENT;
        return result(v, kr == KERN_SUCCESS ? 0 : -1, detail);
    }
#endif
    if (!strcmp(v, "listen-tcp")) {
        int s = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in sa; memset(&sa, 0, sizeof sa);
        sa.sin_family = AF_INET; sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK); sa.sin_port = 0;
        int rc = (s < 0 || bind(s, (struct sockaddr *)&sa, sizeof sa) || listen(s, 4)) ? -1 : 0;
        if (rc == 0) { socklen_t l = sizeof sa; getsockname(s, (struct sockaddr *)&sa, &l); snprintf(detail, sizeof detail, "port=%d", ntohs(sa.sin_port)); }
        result(v, rc, detail);
        if (rc == 0) hold(a2 ? atoi(a2) : 0);
        return rc ? 1 : 0;
    }
    if (!strcmp(v, "listen-unix") && a2) {
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un su; memset(&su, 0, sizeof su);
        su.sun_family = AF_UNIX; strncpy(su.sun_path, a2, sizeof su.sun_path - 1);
        int rc = (s < 0 || bind(s, (struct sockaddr *)&su, sizeof su) || listen(s, 4)) ? -1 : 0;
        result(v, rc, NULL);
        if (rc == 0) hold(a3 ? atoi(a3) : 0);
        return rc ? 1 : 0;
    }
#ifndef NO_NOTIFY
    if (!strcmp(v, "notify-set") && a2 && a3) {
        int tok = 0;
        uint32_t st = notify_register_check(a2, &tok);
        if (st == NOTIFY_STATUS_OK) st = notify_set_state(tok, (uint64_t)strtoull(a3, NULL, 10));
        if (st == NOTIFY_STATUS_OK) notify_post(a2);
        snprintf(detail, sizeof detail, "status=%u", st);
        int rc = st == NOTIFY_STATUS_OK ? 0 : -1;
        result(v, rc, detail);
        if (rc == 0) hold(argc > 4 ? atoi(argv[4]) : 0);
        return rc ? 1 : 0;
    }
    if (!strcmp(v, "notify-get") && a2) {
        int tok = 0; uint64_t st64 = 0;
        uint32_t st = notify_register_check(a2, &tok);
        if (st == NOTIFY_STATUS_OK) st = notify_get_state(tok, &st64);
        snprintf(detail, sizeof detail, "value=%llu", (unsigned long long)st64);
        return result(v, st == NOTIFY_STATUS_OK ? 0 : -1, detail);
    }
#endif
    if (!strcmp(v, "oslog") && a2) {
        os_log(OS_LOG_DEFAULT, "%{public}s", a2);
        return result(v, 0, NULL);
    }
    if (!strcmp(v, "crash-segv")) { volatile int *p = 0; *p = 1; return 0; }
    if (!strcmp(v, "crash-abort")) { abort(); }
    if (!strcmp(v, "hydra") && a2 && a3) {
        /* Every process forks a child every MS ms until N forks happened in all. */
        int n = atoi(a2), ms = atoi(a3);
        int *count = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANON, -1, 0);
        if (count == MAP_FAILED) return result(v, -1, NULL);
        alarm(60);
        result(v, 0, "started");
        for (;;) {
            usleep((useconds_t)ms * 1000);
            if (__sync_fetch_and_add(count, 1) < n) {
                pid_t p = fork();
                if (p < 0) { __sync_fetch_and_sub(count, 1); }
                if (p == 0) alarm(60);
                /* both parent and child continue the loop */
            } else {
                __sync_fetch_and_sub(count, 1);
                /* the cap is reached: keep the process alive until the alarm */
            }
        }
    }
    if (!strcmp(v, "hold-deleted") && a2) {
        int mb = a3 ? atoi(a3) : 16;
        int fd = open(a2, O_CREAT | O_RDWR, 0600);
        if (fd < 0) return result(v, -1, NULL);
        char buf[65536]; memset(buf, 7, sizeof buf);
        for (long i = 0; i < (long)mb * 16; i++) if (write(fd, buf, sizeof buf) < 0) break;
        unlink(a2);
        result(v, 0, "deleted-but-open");
        hold(0);
    }
    if (!strcmp(v, "hold-cwd") && a2) {
        if (chdir(a2)) return result(v, -1, NULL);
        result(v, 0, "cwd");
        hold(0);
    }
    if (!strcmp(v, "hold-lock") && a2) {
        int fd = open(a2, O_CREAT | O_RDWR, 0600);
        if (fd < 0 || flock(fd, LOCK_EX | LOCK_NB)) return result(v, -1, NULL);
        result(v, 0, "locked");
        hold(0);
    }
    if (!strcmp(v, "stopped")) { result(v, 0, "stopping"); raise(SIGSTOP); return 0; }
    if (!strcmp(v, "zombie")) {
        pid_t p = fork();
        if (p == 0) _exit(0);
        result(v, 0, "child-exited-parent-never-waits");
        hold(120);
    }
    fprintf(stderr, "canary: unknown verb %s\n", v);
    return 2;
}
