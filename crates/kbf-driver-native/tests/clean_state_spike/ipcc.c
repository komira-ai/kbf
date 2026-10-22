/* E1d spike helper: POSIX shm and named-semaphore create / unlink / probe, in bulk, with hex-coded name files.
   File format: one "<kind> <hex of name>" per line, kind = shm | sem. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <semaphore.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/resource.h>
#include <sys/wait.h>

static double now_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec * 1000.0 + tv.tv_usec / 1000.0;
}

static const char *pr(const char *n) {
    static char buf[1024];
    size_t j = 0;
    for (size_t i = 0; n[i] && j < sizeof(buf) - 6; i++) {
        unsigned char c = (unsigned char)n[i];
        if (c >= 32 && c < 127 && c != '\\') buf[j++] = (char)c;
        else j += (size_t)snprintf(buf + j, 5, "\\x%02x", c);
    }
    buf[j] = 0;
    return buf;
}

static void hexline(FILE *f, const char *kind, const char *n) {
    fprintf(f, "%s ", kind);
    for (size_t i = 0; n[i]; i++) fprintf(f, "%02x", (unsigned char)n[i]);
    fprintf(f, "\n");
}

static int unhex(const char *h, char *out, size_t cap) {
    size_t n = strlen(h), j = 0;
    for (size_t i = 0; i + 1 < n && j + 1 < cap; i += 2) {
        unsigned int v;
        char b[3] = {h[i], h[i + 1], 0};
        if (sscanf(b, "%2x", &v) != 1) break;
        out[j++] = (char)v;
    }
    out[j] = 0;
    return (int)j;
}

static int do_create(const char *kind, const char *name) {
    if (!strcmp(kind, "shm")) {
        int fd = shm_open(name, O_CREAT | O_RDWR, 0600);
        if (fd < 0) return errno;
        close(fd);
        return 0;
    }
    sem_t *s = sem_open(name, O_CREAT, 0600, 1);
    if (s == SEM_FAILED) return errno;
    sem_close(s);
    return 0;
}

static int do_unlink(const char *kind, const char *name) {
    int r = !strcmp(kind, "shm") ? shm_unlink(name) : sem_unlink(name);
    return r == 0 ? 0 : errno;
}

/* 0 = exists and opened; ENOENT = absent; else the errno (EACCES = exists, not ours) */
static int do_probe(const char *kind, const char *name) {
    if (!strcmp(kind, "shm")) {
        int fd = shm_open(name, O_RDWR, 0);
        if (fd < 0) return errno;
        close(fd);
        return 0;
    }
    sem_t *s = sem_open(name, 0);
    if (s == SEM_FAILED) return errno;
    sem_close(s);
    return 0;
}


static int owner_of(const char *kind, const char *name, unsigned *uid, unsigned *mode) {
    struct stat st;
    int fd;
    if (!strcmp(kind, "shm")) {
        fd = shm_open(name, O_RDONLY, 0);
        if (fd < 0) return errno;
        if (fstat(fd, &st) != 0) { int e = errno; close(fd); return e; }
        close(fd);
    } else {
        sem_t *s = sem_open(name, 0);
        if (s == SEM_FAILED) return errno;
        fd = (int)(intptr_t)s;
        if (fstat(fd, &st) != 0) { int e = errno; sem_close(s); return e; }
        sem_close(s);
    }
    *uid = (unsigned)st.st_uid;
    *mode = (unsigned)(st.st_mode & 07777);
    return 0;
}

static void maybe_euid(void) {
    const char *e = getenv("IPCC_EUID");
    if (e && *e) {
        int r = seteuid((uid_t)atoi(e));
        fprintf(stderr, "seteuid(%s) rc=%d errno=%d\n", e, r, r ? errno : 0);
    }
}

static void record(const char *gt, const char *kind, const char *name) {
    if (!gt || !*gt) return;
    FILE *f = fopen(gt, "a");
    if (!f) return;
    hexline(f, kind, name);
    fclose(f);
}

static void bulk(const char *prefix, int n, const char *outfile) {
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    rl.rlim_cur = rl.rlim_max < 10240 ? rl.rlim_max : 10240;
    setrlimit(RLIMIT_NOFILE, &rl);
    FILE *out = outfile ? fopen(outfile, "a") : NULL;
    const char *kinds[2] = {"shm", "sem"};
    for (int k = 0; k < 2; k++) {
        int ok = 0, fail = 0, first = 0;
        double t0 = now_ms();
        for (int i = 0; i < n; i++) {
            char nm[64];
            snprintf(nm, sizeof nm, "%s%c%d", prefix, k ? 'm' : 's', i);
            int r = do_create(kinds[k], nm);
            if (r == 0) { ok++; if (out) hexline(out, kinds[k], nm); }
            else { fail++; if (!first) first = r; }
        }
        printf("BULK %s prefix=%s n=%d ok=%d fail=%d first_errno=%d(%s) ms=%.0f\n", kinds[k], prefix, n, ok, fail, first,
               first ? strerror(first) : "-", now_ms() - t0);
    }
    if (out) fclose(out);
}

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    const char *m = argv[1];
    if (!strcmp(m, "mk") && argc >= 4) {
        int r = do_create(argv[2], argv[3]);
        printf("MK %s %s rc=%d(%s) euid=%d uid=%d t_ms=%.3f\n", argv[2], pr(argv[3]), r, r ? strerror(r) : "ok", (int)geteuid(),
               (int)getuid(), now_ms());
        if (r == 0 && argc >= 5) record(argv[4], argv[2], argv[3]);
        return r ? 1 : 0;
    }
    if (!strcmp(m, "rm") && argc >= 4) {
        maybe_euid();
        int r = do_unlink(argv[2], argv[3]);
        printf("RM %s %s rc=%d(%s) euid=%d\n", argv[2], pr(argv[3]), r, r ? strerror(r) : "ok", (int)geteuid());
        return r ? 1 : 0;
    }
    if (!strcmp(m, "probe") && argc >= 4) {
        int r = do_probe(argv[2], argv[3]);
        printf("PROBE %s %s %s rc=%d uid=%d euid=%d\n", argv[2], pr(argv[3]), r == 0 ? "EXISTS" : r == ENOENT ? "ENOENT" : r == EACCES ? "EXISTS-EACCES" : "OTHER", r,
               (int)getuid(), (int)geteuid());
        return 0;
    }
    if (!strcmp(m, "owner") && argc >= 4) {
        unsigned u = 0, md = 0;
        int r = owner_of(argv[2], argv[3], &u, &md);
        printf("OWNER %s %s rc=%d(%s) uid=%u mode=%04o\n", argv[2], pr(argv[3]), r, r ? strerror(r) : "ok", u, md);
        return 0;
    }
    if (!strcmp(m, "ownerfile") && argc >= 3) {
        FILE *f = fopen(argv[2], "r");
        if (!f) { printf("OWNERFILE cannot open %s\n", argv[2]); return 1; }
        char line[4096], kind[8], hex[4000], nm[2100];
        unsigned uids[2][64], cnt[2][64];
        int nu[2] = {0, 0}, shown = 0, fails = 0;
        while (fgets(line, sizeof line, f)) {
            if (sscanf(line, "%7s %3999s", kind, hex) != 2) continue;
            unhex(hex, nm, sizeof nm);
            unsigned u = 0, md = 0;
            int r = owner_of(kind, nm, &u, &md);
            int k = !strcmp(kind, "sem");
            if (r != 0) { fails++; if (shown < 12) { printf("OWNER %s %s rc=%d(%s)\n", kind, pr(nm), r, strerror(r)); shown++; } continue; }
            int j = 0;
            while (j < nu[k] && uids[k][j] != u) j++;
            if (j == nu[k] && nu[k] < 64) { uids[k][j] = u; cnt[k][j] = 0; nu[k]++; }
            if (j < nu[k]) cnt[k][j]++;
            if (shown < 12 && argc >= 4) { printf("OWNER %s %s uid=%u mode=%04o\n", kind, pr(nm), u, md); shown++; }
        }
        fclose(f);
        for (int k = 0; k < 2; k++) {
            printf("OWNERS %s:", k ? "sem" : "shm");
            for (int j = 0; j < nu[k]; j++) printf(" uid%u=%u", uids[k][j], cnt[k][j]);
            printf("\n");
        }
        printf("OWNERS fails=%d\n", fails);
        return 0;
    }
    if (!strcmp(m, "lat") && argc >= 4) {
        int r = do_create("shm", argv[2]);
        printf("LAT %s rc=%d t_ms=%.3f\n", argv[2], r, now_ms());
        if (r == 0) record(argv[3], "shm", argv[2]);
        return 0;
    }
    if (!strcmp(m, "loop") && argc >= 5) { bulk(argv[3], atoi(argv[2]), argv[4]); return 0; }
    if (!strcmp(m, "par") && argc >= 6) {
        int procs = atoi(argv[2]), n = atoi(argv[3]);
        for (int i = 0; i < procs; i++) {
            if (fork() == 0) {
                char pfx[64], of[1024];
                snprintf(pfx, sizeof pfx, "%s%d_", argv[4], i);
                snprintf(of, sizeof of, "%s.%d", argv[5], i);
                bulk(pfx, n, of);
                _exit(0);
            }
        }
        while (wait(NULL) > 0) {}
        return 0;
    }
    if (!strcmp(m, "hot") && argc >= 4) {
        int n = atoi(argv[2]);
        double t0 = now_ms();
        int bad = 0;
        for (int i = 0; i < n; i++) {
            sem_t *s = sem_open(argv[3], O_CREAT, 0600, 1);
            if (s == SEM_FAILED) bad++; else sem_close(s);
        }
        printf("HOT %s n=%d bad=%d ms=%.0f\n", argv[3], n, bad, now_ms() - t0);
        return 0;
    }
    if (!strcmp(m, "odd") && argc >= 4) {
        const char *tg = argv[3];
        char nm[512];
        const char *fixed[] = {"sp ace", "tab\tx", "nl\nx", "q\"uo'te", "bs\\x", "pct%s%n", "\xc3\xa9-\xc3\xbc", "(paren)", "semi;x", "star*x", "q?x",
                               "ctl\x01x", "emoji-\xf0\x9f\x98\x80", "dbl//x", "sub/dir", "ipc-posix-sem-create /v",
                               "x\nipc-posix-shm-write /v", "dot.", "..", "trail ", "trailtab\t", "trailnl\n", " lead", "bad\xff\xfe", "esc\x1b[31m", "del\x7f", "lone\xc3",
                               "a\"}\n{\"b", NULL};
        FILE *out = fopen(argv[2], "a");
        for (int i = 0; fixed[i]; i++) {
            for (int k = 0; k < 2; k++) {
                const char *kind = k ? "sem" : "shm";
                snprintf(nm, sizeof nm, "/%s-%s", tg, fixed[i]);
                int r = do_create(kind, nm);
                printf("ODD %s %s rc=%d(%s)\n", kind, pr(nm), r, r ? strerror(r) : "ok");
                if (r == 0 && out) hexline(out, kind, nm);
                snprintf(nm, sizeof nm, "%s-noslash-%d", tg, i);
                r = do_create(kind, nm);
                if (i == 0) printf("ODD %s %s rc=%d(%s)\n", kind, pr(nm), r, r ? strerror(r) : "ok");
                if (r == 0 && out) hexline(out, kind, nm);
            }
        }
        int lens[] = {29, 30, 31, 32, 33, 40, 100, 300};
        for (int j = 0; j < 8; j++) {
            for (int k = 0; k < 2; k++) {
                const char *kind = k ? "sem" : "shm";
                int L = lens[j], i = 0;
                nm[i++] = '/';
                i += snprintf(nm + i, 16, "%s%c", tg, k ? 'm' : 's');
                i += snprintf(nm + i, 16, "L%d_", L);
                while (i < L) nm[i++] = 'x';
                nm[i] = 0;
                int r = do_create(kind, nm);
                printf("ODD %s len=%d rc=%d(%s)\n", kind, (int)strlen(nm), r, r ? strerror(r) : "ok");
                if (r == 0 && out) hexline(out, kind, nm);
            }
        }
        if (out) fclose(out);
        return 0;
    }
    if (!strcmp(m, "rmfile") && argc >= 3) {
        maybe_euid();
        FILE *f = fopen(argv[2], "r");
        if (!f) { printf("RMFILE cannot open %s\n", argv[2]); return 1; }
        char line[4096], kind[8], hex[4000], nm[2100];
        int ok = 0, enoent = 0, other = 0, first = 0;
        double t0 = now_ms();
        while (fgets(line, sizeof line, f)) {
            if (sscanf(line, "%7s %3999s", kind, hex) != 2) continue;
            unhex(hex, nm, sizeof nm);
            int r = do_unlink(kind, nm);
            if (r == 0) ok++; else if (r == ENOENT) enoent++; else { other++; if (!first) first = r; }
        }
        fclose(f);
        printf("RMFILE ok=%d enoent=%d other=%d first_errno=%d(%s) ms=%.0f euid=%d\n", ok, enoent, other, first, first ? strerror(first) : "-", now_ms() - t0, (int)geteuid());
        return 0;
    }
    if (!strcmp(m, "probefile") && argc >= 3) {
        FILE *f = fopen(argv[2], "r");
        if (!f) { printf("PROBEFILE cannot open %s\n", argv[2]); return 1; }
        FILE *out = argc >= 4 ? fopen(argv[3], "w") : NULL;
        char line[4096], kind[8], hex[4000], nm[2100];
        int ex = 0, no = 0, other = 0, total = 0;
        double t0 = now_ms();
        while (fgets(line, sizeof line, f)) {
            if (sscanf(line, "%7s %3999s", kind, hex) != 2) continue;
            unhex(hex, nm, sizeof nm);
            int r = do_probe(kind, nm);
            total++;
            if (r == 0 || r == EACCES) { ex++; if (out) hexline(out, kind, nm); }
            else if (r == ENOENT) no++; else other++;
        }
        fclose(f);
        if (out) fclose(out);
        printf("PROBEFILE total=%d exists=%d enoent=%d other=%d ms=%.0f uid=%d\n", total, ex, no, other, now_ms() - t0, (int)getuid());
        return 0;
    }
    fprintf(stderr, "usage\n");
    return 2;
}
