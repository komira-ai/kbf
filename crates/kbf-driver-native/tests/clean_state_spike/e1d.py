# E1d analysis helper (spike). Run as: /usr/bin/python3 -I e1d.py <cmd> ...
# Name files: "<kind> <hex name>" per line (kind = shm | sem).
import sys, json, re, datetime

OPRE = re.compile(r'ipc-posix-(shm|sem)([A-Za-z*-]*)')
PIDRE = re.compile(r'^(?:Sandbox:\s*)?(.*?)\((\d+)\)')


def hx(s):
    return s.encode('utf-8', 'surrogateescape').hex() if isinstance(s, str) else s.hex()


def pr(h):
    b = bytes.fromhex(h)
    return ''.join(chr(c) if 32 <= c < 127 and c != 92 else '\\x%02x' % c for c in b)


def load_json(path, stream=False):
    out = []
    try:
        f = open(path, 'rb')
    except OSError:
        return out
    with f:
        for raw in f:
            line = raw.decode('utf-8', 'replace')
            t = None
            if stream:
                tab = line.find('\t')
                if tab < 0:
                    continue
                try:
                    t = float(line[:tab])
                except ValueError:
                    continue
                line = line[tab + 1:]
            line = line.strip()
            if not line.startswith('{'):
                continue
            try:
                j = json.loads(line)
            except Exception:
                continue
            out.append((t, j))
    return out


def epoch(ts):
    # "2026-10-22 12:00:00.123456+0000"
    try:
        s = ts
        m = re.match(r'(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)(\.\d+)?([+-]\d{4})?', s)
        d = datetime.datetime.strptime(m.group(1) + (m.group(3) or '+0000'), '%Y-%m-%d %H:%M:%S%z')
        return d.timestamp() + float(m.group(2) or 0)
    except Exception:
        return 0.0


def parse_msg(j):
    """-> (verdict, op, kind, name_bytes_hex, pid, proc) or None"""
    msg = j.get('eventMessage', '')
    m = OPRE.search(msg)
    if not m:
        return None
    kind = m.group(1)
    op = m.group(0)
    pre = msg[:m.start()]
    name = msg[m.end():]
    if name.startswith(' '):
        name = name[1:]
    verdict = 'allow' if re.search(r'\ballow', pre) else ('deny' if re.search(r'\bdeny', pre) else '?')
    pid, proc = '', ''
    pm = PIDRE.match(pre)
    if pm:
        proc, pid = pm.group(1).strip(), pm.group(2)
    return verdict, op, kind, name.encode('utf-8', 'surrogateescape').hex(), pid, proc


def cmd_raw(path, n):
    recs = load_json(path)
    print('RAW records=%d' % len(recs))
    for t, j in recs[:int(n)]:
        keep = {k: j.get(k) for k in ('timestamp', 'processID', 'processImagePath', 'senderImagePath', 'subsystem', 'category',
                                      'messageType', 'eventMessage', 'eventType') if k in j}
        print('RAWREC ' + json.dumps(keep, ensure_ascii=True)[:420])
    print('RAWKEYS ' + ','.join(sorted(recs[0][1].keys())) if recs else 'RAWKEYS none')


def cmd_parse(path, out, stream):
    recs = load_json(path, stream == 'stream')
    n = 0
    with open(out, 'w') as f:
        for t, j in recs:
            p = parse_msg(j)
            if not p:
                continue
            ts = epoch(j.get('timestamp', ''))
            f.write('\t'.join([('%.6f' % ts), ('%.3f' % t) if t else '', p[4], p[5], p[0], p[1], p[2], p[3]]) + '\n')
            n += 1
    print('PARSE records=%d reports=%d' % (len(recs), n))


def load_tsv(path):
    rows = []
    try:
        for l in open(path):
            c = l.rstrip('\n').split('\t')
            if len(c) >= 8:
                rows.append(dict(ts=float(c[0]), arr=c[1], pid=c[2], proc=c[3], verdict=c[4], op=c[5], kind=c[6], hexname=c[7]))
    except OSError:
        pass
    return rows


def load_gt(path):
    s = set()
    try:
        for l in open(path):
            p = l.split()
            if len(p) == 2:
                s.add((p[0], p[1]))
    except OSError:
        pass
    return s


def cmd_ops(tsv):
    rows = load_tsv(tsv)
    c = {}
    for r in rows:
        k = (r['verdict'], r['op'])
        c[k] = c.get(k, 0) + 1
    for k, v in sorted(c.items(), key=lambda x: -x[1])[:12]:
        print('OPS %s %s %d' % (k[0], k[1], v))
    pc = {}
    for r in rows:
        pc[r['proc']] = pc.get(r['proc'], 0) + 1
    print('PROCS ' + ' '.join('%s=%d' % kv for kv in sorted(pc.items(), key=lambda x: -x[1])[:8]))


def cmd_cmp(tsv, gtfile, tag):
    rows = load_tsv(tsv)
    anyr, crer = set(), set()
    for r in rows:
        key = (r['kind'], r['hexname'])
        anyr.add(key)
        if 'create' in r['op']:
            crer.add(key)
    gt = load_gt(gtfile)
    for kind in ('shm', 'sem'):
        t = {x for x in gt if x[0] == kind}
        if not t:
            continue
        miss_any = sorted(x for x in t if x not in anyr)
        miss_cr = [x for x in t if x not in crer]
        print('CMP %s %s truth=%d reported_any=%d reported_create=%d missing_any=%d missing_create=%d' %
              (tag, kind, len(t), len(t) - len(miss_any), len(t) - len(miss_cr), len(miss_any), len(miss_cr)))
        for x in miss_any[:6]:
            print('CMPMISS %s %s %s' % (tag, kind, pr(x[1])[:90]))


def cmd_union(out, *ins):
    s = set()
    for p in ins:
        s |= load_gt(p)
    with open(out, 'w') as f:
        for k, h in sorted(s):
            f.write('%s %s\n' % (k, h))
    print('UNION %d' % len(s))


def cmd_harvest(tsv, out, mode):
    rows = sorted(load_tsv(tsv), key=lambda r: r['ts'])
    live = {}
    for r in rows:
        key = (r['kind'], r['hexname'])
        if mode == 'any':
            live[key] = 1
        elif 'create' in r['op'] and r['verdict'] != 'deny':
            live[key] = 1
        elif 'unlink' in r['op']:
            live.pop(key, None)
    with open(out, 'w') as f:
        for k, h in sorted(live):
            f.write('%s %s\n' % (k, h))
    print('HARVEST mode=%s names=%d' % (mode, len(live)))


def cmd_dt(dtfile, creates_out, all_out):
    cre, allv = set(), set()
    ev = []
    try:
        lines = open(dtfile, 'rb').read().decode('utf-8', 'replace').split('\n')
    except OSError:
        lines = []
    for l in lines:
        p = l.split(' ', 4)
        if len(p) < 5 or p[0] not in ('shm_open', 'sem_open', 'shm_unlink', 'sem_unlink'):
            continue
        kind = p[0][:3]
        try:
            flags = int(p[1])
        except ValueError:
            continue
        key = (kind, p[4].encode('utf-8', 'surrogateescape').hex())
        ev.append((p[0], flags, p[2], p[3], key))
        if p[0].endswith('_open'):
            allv.add(key)
            if flags & 0x200:
                cre.add(key)
    for out, s in ((creates_out, cre), (all_out, allv)):
        with open(out, 'w') as f:
            for k, h in sorted(s):
                f.write('%s %s\n' % (k, h))
    execs = {}
    for fn, fl, ex, pid, key in ev:
        execs[ex] = execs.get(ex, 0) + 1
    print('DT events=%d creates=%d opens=%d by_exec=%s' % (len(ev), len(cre), len(allv),
          ' '.join('%s=%d' % kv for kv in sorted(execs.items(), key=lambda x: -x[1])[:10])))


def cmd_unrep(dtfile, tsv, tag):
    """created per dtrace by which executables, and not reported at all"""
    rep = {(r['kind'], r['hexname']) for r in load_tsv(tsv)}
    byexec = {}
    try:
        lines = open(dtfile, 'rb').read().decode('utf-8', 'replace').split('\n')
    except OSError:
        lines = []
    for l in lines:
        p = l.split(' ', 4)
        if len(p) < 5 or not p[0].endswith('_open'):
            continue
        try:
            fl = int(p[1])
        except ValueError:
            continue
        if not fl & 0x200:
            continue
        key = (p[0][:3], p[4].encode('utf-8', 'surrogateescape').hex())
        d = byexec.setdefault(p[2], [0, 0, []])
        d[0] += 1
        if key not in rep:
            d[1] += 1
            if len(d[2]) < 3:
                d[2].append(pr(key[1])[:40])
    for ex, (tot, un, ex_names) in sorted(byexec.items(), key=lambda x: -x[1][0])[:15]:
        print('UNREP %s exec=%s create_calls=%d unreported=%d e.g. %s' % (tag, ex, tot, un, ex_names))


def cmd_extra(tsv, tag, *gts):
    gt = set()
    for p in gts:
        gt |= load_gt(p)
    rep = {(r['kind'], r['hexname']) for r in load_tsv(tsv) if 'create' in r['op']}
    ex = sorted(x for x in rep if x not in gt)
    print('EXTRA %s reported_create_not_in_truth=%d' % (tag, len(ex)))
    for x in ex[:6]:
        print('EXTRAN %s %s %s' % (tag, x[0], pr(x[1])[:100]))


def cmd_pidcov(tsv, poll, tag):
    seen = set()
    try:
        for l in open(poll):
            c = l.split()
            if len(c) >= 2:
                seen.add(c[1])
    except OSError:
        pass
    rows = load_tsv(tsv)
    pids = {r['pid'] for r in rows if r['pid']}
    inn = {p for p in pids if p in seen}
    lines_in = sum(1 for r in rows if r['pid'] in seen)
    print('PIDCOV %s reporting_pids=%d seen_by_poll=%d lines=%d lines_from_seen_pids=%d' % (tag, len(pids), len(inn), len(rows), lines_in))
    miss = sorted({(r['pid'], r['proc']) for r in rows if r['pid'] not in seen})[:8]
    print('PIDMISS %s %s' % (tag, miss))


def cmd_lat(stream_tsv, latgt_times, seen_file):
    """latgt_times: file with lines 'name t_ms' (creation); seen_file: 'name epoch_s' (log-show watcher)"""
    created = {}
    for l in open(latgt_times):
        p = l.split()
        if len(p) == 2:
            created[p[0]] = float(p[1]) / 1000.0
    seen = {}
    try:
        for l in open(seen_file):
            p = l.split()
            if len(p) == 2:
                seen.setdefault(p[0], float(p[1]))
    except OSError:
        pass
    arr, ev = {}, {}
    for r in load_tsv(stream_tsv):
        try:
            nm = bytes.fromhex(r['hexname']).decode('utf-8', 'replace')
        except ValueError:
            continue
        if 'create' in r['op'] and r['arr']:
            arr.setdefault(nm, (float(r['arr']), r['ts']))
    for n, t in sorted(created.items()):
        a = arr.get(n)
        print('LAT %s stream_arrival_s=%s event_ts_minus_create_s=%s logshow_seen_s=%s' % (
            n, ('%.2f' % (a[0] - t)) if a else 'MISSING', ('%.2f' % (a[1] - t)) if a else '-',
            ('%.1f' % (seen[n] - t)) if n in seen else 'not-seen'))


def cmd_sub(out, a, *bs):
    s = load_gt(a)
    for b in bs:
        s -= load_gt(b)
    with open(out, 'w') as f:
        for k, h in sorted(s):
            f.write('%s %s\n' % (k, h))
    print('SUB %d' % len(s))


def cmd_show(path, n):
    for k, h in sorted(load_gt(path))[:int(n)]:
        print('NAME %s %s' % (k, pr(h)[:90]))


def cmd_tok(tsv, token):
    rows = load_tsv(tsv)
    tot = 0
    names = set()
    for r in rows:
        try:
            nm = bytes.fromhex(r['hexname']).decode('utf-8', 'replace')
        except ValueError:
            continue
        if token in nm:
            tot += 1
            names.add(nm)
    print('TOK %s lines=%d distinct_names=%d' % (token, tot, len(names)))


if __name__ == '__main__':
    c = sys.argv[1]
    globals()['cmd_' + c](*sys.argv[2:])
