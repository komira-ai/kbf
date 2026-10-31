// The publish program (tools/publish.ts), run against stand-ins for the runner's token service and the `aws` CLI.
//
// Catches: a command line accepted with a flag missing, repeated, unknown or empty (an unset repository variable is an
// empty flag); a plan that deletes before it has uploaded, deletes or writes under releases/, sends anything to another
// bucket, or invalidates before the upload is done; the account id (inside the role ARN and the bucket name) reaching a
// log line unmasked, or masked after the first command; the OIDC token on a command line; temporary credentials in the
// environment of the call that fetches them, or any `aws` call after a failed exchange; a dist/ that is not a built site
// reaching the token service.

import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { checkDist, type Deps, IMMUTABLE, type Options, PAGE, parseArgs, plan, publish } from '../tools/publish';
import { scratch } from './support';

const ACCOUNT = '123456789012';
const GOOD: Options = {
  dist: 'dist',
  bucket: `site-bucket-${ACCOUNT}`,
  distributionId: 'E1ABCDEFGHIJKL',
  roleArn: `arn:aws:iam::${ACCOUNT}:role/site-publish`,
  region: 'us-east-1',
  commit: '0123456789abcdef0123456789abcdef01234567',
};
const JWT = 'aaaa.bbbb.cccc';
const CREDS = JSON.stringify({ AccessKeyId: 'access-key-example', SecretAccessKey: 'secret-example', SessionToken: 'session-example' });

function argvOf(o: Options): string[] {
  return ['--dist', o.dist, '--bucket', o.bucket, '--distribution-id', o.distributionId, '--role-arn', o.roleArn, '--region', o.region, '--commit', o.commit];
}

/** A built-looking dist/ in a scratch directory. */
function fakeDist(name: string): string {
  const dist = join(scratch(name), 'dist');
  mkdirSync(join(dist, 'assets', 'x'), { recursive: true });
  writeFileSync(join(dist, 'index.html'), '<p>');
  writeFileSync(join(dist, '404.html'), '<p>');
  return dist;
}

interface Call {
  argv: string[];
  env: Record<string, string>;
}

/** Stand-ins; `events` is every log line and call in the order they happened. */
function fake(opts: { sts?: string | Error; jwt?: string; failAt?: number } = {}) {
  const events: string[] = [];
  const calls: Call[] = [];
  const secrets = new Map<string, string>();
  let tokenAsked = 0;
  const deps: Deps = {
    token: async () => {
      tokenAsked++;
      events.push('token');
      return opts.jwt ?? JWT;
    },
    writeSecret: (text) => {
      const path = `/secret/${secrets.size}`;
      secrets.set(path, text);
      return path;
    },
    writeVersion: (commit) => `/version/${commit}`,
    run: (argv, env) => {
      calls.push({ argv: [...argv], env: { ...env } });
      events.push(`run ${argv.slice(0, 2).join(' ')}`);
      if (argv[0] === 'sts') {
        if (opts.sts instanceof Error) throw opts.sts;
        return opts.sts ?? CREDS;
      }
      if (opts.failAt === calls.length) throw new Error('aws failed');
      return '';
    },
    log: (line) => events.push(`log ${line}`),
  };
  return { deps, events, calls, secrets, tokenAsked: () => tokenAsked };
}

test('the command line must give every flag once, with a value of the right shape', () => {
  assert.deepEqual(parseArgs(argvOf(GOOD)), GOOD);
  const refused: [string[], RegExp][] = [
    [[], /--dist is required|--bucket is required/],
    [[...argvOf(GOOD), '--bucket', 'other'], /--bucket is given twice/],
    [[...argvOf(GOOD), '--acl', 'public-read'], /unknown argument --acl/],
    [[...argvOf(GOOD).slice(0, -1)], /--commit needs a value/],
    [argvOf({ ...GOOD, bucket: '' }), /--bucket is empty or malformed/],
    [argvOf({ ...GOOD, bucket: 'Has/Slash' }), /--bucket is empty or malformed/],
    [argvOf({ ...GOOD, distributionId: 'e1abc' }), /--distribution-id is empty or malformed/],
    [argvOf({ ...GOOD, roleArn: '' }), /--role-arn is empty or malformed/],
    [argvOf({ ...GOOD, roleArn: `arn:aws:iam::${ACCOUNT}:user/someone` }), /--role-arn is empty or malformed/],
    [argvOf({ ...GOOD, roleArn: `arn:aws:iam::12345:role/x` }), /--role-arn is empty or malformed/],
    [argvOf({ ...GOOD, region: 'moon' }), /--region is empty or malformed/],
    [argvOf({ ...GOOD, commit: 'abc123' }), /--commit is empty or malformed/],
    [argvOf({ ...GOOD, dist: '' }), /--dist is empty or malformed/],
  ];
  for (const [argv, why] of refused) assert.throws(() => parseArgs(argv), why, argv.join(' '));
});

test('the plan uploads assets, then pages, then the version, then sweeps, then invalidates', () => {
  const p = plan(GOOD, '/v');
  const verbs = p.map((a) => a.slice(0, 2).join(' '));
  assert.deepEqual(verbs, ['s3 sync', 's3 sync', 's3 sync', 's3 cp', 's3 sync', 'cloudfront create-invalidation']);
  // Only the sweep deletes, and it is the last thing before the invalidation.
  const deleting = p.map((a, i) => (a.includes('--delete') ? i : -1)).filter((i) => i >= 0);
  assert.deepEqual(deleting, [4]);
  assert.deepEqual(p[5], ['cloudfront', 'create-invalidation', '--distribution-id', GOOD.distributionId, '--paths', '/*']);
  // Assets are immutable, pages are not.
  assert.ok(p[0].includes(IMMUTABLE) && p[1].includes(IMMUTABLE));
  assert.ok(p[2].includes(PAGE) && !p[2].includes(IMMUTABLE));
});

test('no command writes to another bucket, and none can delete or overwrite under releases/', () => {
  for (const argv of plan(GOOD, '/v')) {
    const s3 = argv.filter((a) => a.startsWith('s3://'));
    for (const target of s3) assert.ok(target === `s3://${GOOD.bucket}/` || target === `s3://${GOOD.bucket}/version.txt`, target);
    if (argv[0] === 's3' && argv[1] === 'sync') {
      // The last filter on the command line wins, and it keeps releases/ out of every sync (a delete included).
      const filters = argv.flatMap((x, i) => (x === '--exclude' || x === '--include' ? [`${x} ${argv[i + 1]}`] : []));
      const exclude = filters.lastIndexOf('--exclude releases/*');
      assert.ok(exclude >= 0, argv.join(' '));
      filters.forEach((f, i) => {
        if (f.startsWith('--include')) assert.ok(i < exclude, `an include after the releases/ exclude: ${argv.join(' ')}`);
      });
    }
    assert.equal(argv.includes('--acl'), false);
    assert.equal(argv.includes('--force'), false);
  }
  // The sweep must not delete version.txt, which is not in dist/.
  const sweep = plan(GOOD, '/v')[4];
  assert.ok(sweep.join(' ').includes('--exclude version.txt'));
});

test('a publish masks the account id, the names and the token before anything runs, and keeps the token off the command line', async () => {
  const dist = fakeDist('publish-ok');
  const g = fake();
  await publish({ ...GOOD, dist }, g.deps);
  const firstRun = g.events.findIndex((e) => e.startsWith('run '));
  const masked = g.events.slice(0, firstRun).filter((e) => e.startsWith('log ::add-mask::')).map((e) => e.slice('log ::add-mask::'.length));
  for (const v of [GOOD.roleArn, GOOD.bucket, GOOD.distributionId, ACCOUNT, JWT]) assert.ok(masked.includes(v), `${v} is masked before the first command`);
  // The credentials are masked before the first command that carries them.
  const firstS3 = g.events.findIndex((e) => e.startsWith('run s3'));
  const maskedBeforeS3 = g.events.slice(0, firstS3).filter((e) => e.startsWith('log ::add-mask::'));
  for (const v of ['access-key-example', 'secret-example', 'session-example']) assert.ok(maskedBeforeS3.includes(`log ::add-mask::${v}`), v);
  // No log line shows an unmasked name: the only lines that carry them are the masks.
  for (const e of g.events.filter((x) => x.startsWith('log ') && !x.startsWith('log ::add-mask::'))) {
    for (const v of [ACCOUNT, JWT, 'secret-example', 'session-example']) assert.equal(e.includes(v), false, e);
  }
  // The token is in a file and in no argument.
  const sts = g.calls[0];
  assert.equal(sts.argv[0], 'sts');
  assert.equal(sts.argv.some((a) => a.includes(JWT)), false);
  const at = sts.argv.indexOf('--web-identity-token');
  assert.equal(g.secrets.get(sts.argv[at + 1].replace('file://', '')), JWT);
  assert.equal(sts.argv[sts.argv.indexOf('--role-arn') + 1], GOOD.roleArn);
});

test('the credentials reach only the calls after the exchange, and the exchange carries none', async () => {
  const dist = fakeDist('publish-env');
  const f = fake();
  await publish({ ...GOOD, dist }, f.deps);
  const [sts, ...rest] = f.calls;
  assert.deepEqual(Object.keys(sts.env).sort(), ['AWS_DEFAULT_REGION']);
  assert.equal(rest.length, plan(GOOD, '/v').length);
  assert.ok(sts.argv.includes('assume-role-with-web-identity'));
  for (const c of rest) {
    assert.equal(c.env.AWS_ACCESS_KEY_ID, 'access-key-example');
    assert.equal(c.env.AWS_SECRET_ACCESS_KEY, 'secret-example');
    assert.equal(c.env.AWS_SESSION_TOKEN, 'session-example');
    assert.equal(c.env.AWS_DEFAULT_REGION, 'us-east-1');
    assert.equal(c.argv.some((a) => a.includes('secret-example') || a.includes('session-example')), false);
  }
  assert.deepEqual(rest.map((c) => c.argv), plan({ ...GOOD, dist }, `/version/${GOOD.commit}`));
});

test('a failed exchange or malformed credentials end the publish before any upload', async () => {
  const dist = fakeDist('publish-sts');
  for (const sts of [new Error('AccessDenied'), 'not json', JSON.stringify({ AccessKeyId: 'a', SecretAccessKey: 'b' }), JSON.stringify({ AccessKeyId: 'a', SecretAccessKey: 'b', SessionToken: '' })]) {
    const f = fake({ sts });
    await assert.rejects(publish({ ...GOOD, dist }, f.deps));
    assert.deepEqual(f.calls.map((c) => c.argv[0]), ['sts'], String(sts));
  }
});

test('a failing upload stops the publish: nothing after it runs, so the invalidation never follows a half upload', async () => {
  const dist = fakeDist('publish-fail');
  const f = fake({ failAt: 3 });
  await assert.rejects(publish({ ...GOOD, dist }, f.deps), /aws failed/);
  assert.equal(f.calls.length, 3);
  assert.equal(f.calls.some((c) => c.argv[0] === 'cloudfront'), false);
  assert.equal(f.calls.some((c) => c.argv.includes('--delete')), false);
});

test('a token that is not a JWT, and a dist/ that is not a built site, end the publish before any aws call', async () => {
  const dist = fakeDist('publish-jwt');
  for (const jwt of ['', 'no-dots', 'a.b', '<html>']) {
    const f = fake({ jwt });
    await assert.rejects(publish({ ...GOOD, dist }, f.deps), /no JWT/);
    assert.equal(f.calls.length, 0, jwt);
  }
  const empty = join(scratch('publish-empty'), 'dist');
  mkdirSync(empty, { recursive: true });
  const f = fake();
  await assert.rejects(publish({ ...GOOD, dist: empty }, f.deps), /not a built site/);
  assert.equal(f.tokenAsked(), 0, 'the token service is not asked for a dist/ that would not be uploaded');
  assert.equal(f.calls.length, 0);
});

test('checkDist needs index.html, 404.html and assets/', () => {
  checkDist(fakeDist('publish-check'));
  for (const gone of ['index.html', '404.html']) {
    const only = join(scratch(`publish-check-no-${gone}`), 'dist');
    mkdirSync(join(only, 'assets'), { recursive: true });
    for (const keep of ['index.html', '404.html'].filter((x) => x !== gone)) writeFileSync(join(only, keep), '<p>');
    assert.throws(() => checkDist(only), new RegExp(`${gone} is missing`));
  }
  const noAssets = join(scratch('publish-check-no-assets'), 'dist');
  mkdirSync(noAssets, { recursive: true });
  writeFileSync(join(noAssets, 'index.html'), '<p>');
  writeFileSync(join(noAssets, '404.html'), '<p>');
  assert.throws(() => checkDist(noAssets), /assets\/ is missing/);
});
