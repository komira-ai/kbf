# E1h analysis helper (spike). Run as root-side tooling: /usr/bin/python3 -I e1h.py <cmd> ...
# (the SUBJECT interpreter of the experiment is the pinned python-build-standalone one; this file only reads logs.)
# The dtrace record has one event per line: "<ms>|<uid>|<probefunc>|<arg1>|<execname>|<pid>|<name>".
import sys, re, collections

O_CREAT = 0x200


def load(dt):
    ev = []
    try:
        data = open(dt, 'rb').read().decode('utf-8', 'replace')
    except OSError:
        return ev
    for l in data.split('\n'):
        p = l.split('|', 6)
        if len(p) < 7:
            continue
        try:
            ev.append((float(p[0]) / 1000.0, int(p[1]), p[2], int(p[3]), p[4], int(p[5]), p[6]))
        except ValueError:
            continue
    return ev


def hx(s):
    return s.encode('utf-8', 'surrogateescape').hex()


def is_py(exe):
    return exe.startswith('python')


def creates(ev, uid, t0, t1, who):
    out = {}
    for (t, u, fn, fl, exe, pid, name) in ev:
        if u != uid or t < t0 or t > t1:
            continue
        if not fn.endswith('_open') or not (fl & O_CREAT):
            continue
        py = is_py(exe)
        if who == 'py' and not py:
            continue
        if who == 'apple' and py:
            continue
        out.setdefault((fn[:3], name), (t, exe, pid))
    return out


def cmd_names(dt, uid, t0, t1, who, outf):
    c = creates(load(dt), int(uid), float(t0), float(t1), who)
    with open(outf, 'w') as f:
        for (k, n) in sorted(c):
            f.write('%s %s\n' % (k, hx(n)))
    print('NAMES who=%s uid=%s n=%d' % (who, uid, len(c)))


def cmd_events(dt, uid, t0, t1, tterm):
    """Python names created in the window and who unlinked them, relative to the clean's SIGTERM."""
    uid, t0, t1, tterm = int(uid), float(t0), float(t1), float(tterm)
    ev = load(dt)
    c = creates(ev, uid, t0, t1, 'py')
    unl = collections.defaultdict(list)
    for (t, u, fn, fl, exe, pid, name) in ev:
        if u == uid and fn.endswith('_unlink') and t >= t0 and t <= t1 + 5:
            unl[(fn[:3], name)].append((t, exe, pid))
    by_creator = before_term = after_term = never = 0
    creators = collections.Counter()
    samples = []
    for key, (tc, exe, pid) in sorted(c.items(), key=lambda kv: kv[1][0]):
        creators[exe] += 1
        us = [x for x in unl.get(key, []) if x[0] >= tc]
        if not us:
            never += 1
            samples.append('NEVER-UNLINKED %s %s created_by=%s/%d' % (key[0], key[1], exe, pid))
            continue
        t, e2, p2 = us[0]
        if p2 == pid:
            by_creator += 1
        elif t < tterm:
            before_term += 1
            if len(samples) < 6:
                samples.append('OTHER-PID-BEFORE-TERM %s %s created_by=%d unlinked_by=%s/%d dt=%.2fs' % (key[0], key[1], pid, e2, p2, t - tterm))
        else:
            after_term += 1
            if len(samples) < 6:
                samples.append('OTHER-PID-AFTER-TERM %s %s created_by=%d unlinked_by=%s/%d dt=%.2fs' % (key[0], key[1], pid, e2, p2, t - tterm))
    print('PYLIFE created=%d unlinked_by_creator=%d unlinked_by_other_before_term=%d unlinked_by_other_after_term=%d never_unlinked=%d creators=%s' % (
        len(c), by_creator, before_term, after_term, never, ','.join('%s=%d' % kv for kv in creators.items())))
    for s in samples[:8]:
        print('PYEV ' + s)


def unhex(h):
    return bytes.fromhex(h).decode('utf-8', 'replace')


def cmd_show(path, limit):
    n = 0
    try:
        lines = open(path).read().split('\n')
    except OSError:
        lines = []
    for l in lines:
        p = l.split(' ')
        if len(p) != 2:
            continue
        print('%s %s' % (p[0], unhex(p[1])))
        n += 1
        if n >= int(limit):
            break


def norm(name):
    s = re.sub(r'[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}', '<UUID>', name)
    s = re.sub(r'sim\.not\.[A-Z0-9]+', 'sim.not.<RAND>', s)
    s = re.sub(r'\d{3,}', '<N>', s)
    return s


def cmd_pat(path, tag):
    """The name patterns of a name file (uuid/number runs masked), by count."""
    c = collections.Counter()
    try:
        lines = open(path).read().split('\n')
    except OSError:
        lines = []
    for l in lines:
        p = l.split(' ')
        if len(p) == 2:
            c[(p[0], norm(unhex(p[1])))] += 1
    for (k, n), cnt in c.most_common(30):
        print('HPAT %s %d %s %s' % (tag, cnt, k, n))
    print('HPAT %s total=%d patterns=%d' % (tag, sum(c.values()), len(c)))


def kv(line):
    d = {}
    for tok in line.split():
        if '=' in tok:
            a, b = tok.split('=', 1)
            d[a] = b
    return d


def num(r, f):
    try:
        return int(r.get(f, '0'))
    except ValueError:
        return 0


def cmd_sum(path):
    rows = []
    try:
        for l in open(path).read().split('\n'):
            if l.startswith('HLEASE '):
                rows.append(kv(l))
    except OSError:
        pass
    g = collections.OrderedDict()
    for r in rows:
        k = (r.get('arm', '?'), r.get('mode', '?'))
        a = g.setdefault(k, collections.Counter())
        a['leases'] += 1
        for f in ('py_created', 'py_left_term', 'py_left_final', 'py_left_kp3'):
            a[f] += num(r, f)
        if num(r, 'py_left_final') > 0:
            a['leases_with_py_left'] += 1
    for (arm, mode), a in g.items():
        print('HSUM arm=%s mode=%s leases=%d py_created=%d left_after_kp_parent_kill_3s=%d left_after_term_grace=%d left_final=%d leases_with_py_left=%d' % (
            arm, mode, a['leases'], a['py_created'], a['py_left_kp3'], a['py_left_term'], a['py_left_final'], a['leases_with_py_left']))
    byuid = collections.OrderedDict()
    for r in rows:
        byuid.setdefault(r.get('uid', '?'), []).append(r)
    for uid, rs in byuid.items():
        cc = ['%s:%s:%s' % (r.get('n'), r.get('arm'), r.get('apple_cum_created', '?')) for r in rs]
        ce = [r.get('apple_cum_existing', '?') for r in rs]
        print('HPLATEAU uid=%s leases=%d cum_existing_by_lease=%s' % (uid, len(rs), ','.join(ce)))
        print('HPLATEAU uid=%s cum_created_by_lease(n:arm:count)=%s' % (uid, ','.join(cc)))


def cmd_stages(path):
    """One line per simulator lease: lease-created Apple names still existing after each stage."""
    try:
        for l in open(path).read().split('\n'):
            if l.startswith('HLEASE ') and 'sim_booted=' in l:
                r = kv(l)
                print('HSTAGES n=%s arm=%s mode=%s uid=%s lease_created=%s booted=%s after_shutdown=%s after_erase=%s after_clean=%s' % (
                    r.get('n'), r.get('arm'), r.get('mode'), r.get('uid'), r.get('lease_created'), r.get('sim_booted'),
                    r.get('sim_shutdown'), r.get('sim_erase'), r.get('sim_clean')))
    except OSError:
        pass


if __name__ == '__main__':
    c = sys.argv[1]
    a = sys.argv[2:]
    {'names': cmd_names, 'events': cmd_events, 'show': cmd_show, 'pat': cmd_pat, 'sum': cmd_sum, 'stages': cmd_stages}[c](*a)
