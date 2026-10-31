// Publishes a built dist/ to the tierstore.dev bucket: the one program the deploy workflow
// (.github/workflows/tierstore-site-publish.yml) runs once the site has been built and tested.
//
//   node tools/publish.ts --dist <dir> --bucket <name> --distribution-id <id> --role-arn <arn> \
//        --region <region> --commit <40-hex sha>
//
// What it does, in order:
//  1. refuses a dist/ that is not a built site, and a flag that is missing, repeated, unknown or malformed, before it
//     asks anyone for anything;
//  2. asks the runner's token service for the job's OIDC token (audience sts.amazonaws.com), writes it to a file only
//     the job's user can read (never on a command line), and exchanges it with `aws sts assume-role-with-web-identity`
//     for credentials that last an hour. There is no long-lived key anywhere;
//  3. uploads the assets (they never change under a URL, so they are immutable), then the pages (short cache), then
//     version.txt, then sweeps what the build no longer has, then invalidates the distribution. The sweep comes after
//     the uploads, so a visitor holding an old page can still load the old assets until the new pages are in place.
//     releases/ is never touched by any command here (the role also denies deleting it).
//
// The repository is public and so are its Actions logs, and the bucket name and the role carry the account id, so
// every such value is masked (`::add-mask::`) before the first command runs.
//
// This file is run from source: the deploy job checks out this file alone, installs nothing, and Node strips the types.
// So it imports `node:` built-ins only (a test holds that), and its syntax is what stripping erases (tsconfig:
// erasableSyntaxOnly). Code that ran in the build job is never what holds the credentials.
//
// Configuration is flags. The two variables the runner sets for a job that may ask for a token
// (ACTIONS_ID_TOKEN_REQUEST_URL and ACTIONS_ID_TOKEN_REQUEST_TOKEN) and RUNNER_TEMP are the platform's own; nothing else
// is read from the environment, and the child `aws` processes get a fresh one that holds the credentials and nothing
// the job had.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, relative, sep } from 'node:path';

export interface Options {
  dist: string;
  bucket: string;
  distributionId: string;
  roleArn: string;
  region: string;
  commit: string;
}

const FLAGS = new Map<string, keyof Options>([
  ['--dist', 'dist'],
  ['--bucket', 'bucket'],
  ['--distribution-id', 'distributionId'],
  ['--role-arn', 'roleArn'],
  ['--region', 'region'],
  ['--commit', 'commit'],
]);

/** What each value may look like. An empty value (an unset repository variable) matches none of them. */
export const SHAPES: Readonly<Record<keyof Options, RegExp>> = {
  dist: /^[A-Za-z0-9._/-]+$/,
  bucket: /^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/,
  distributionId: /^E[A-Z0-9]{8,20}$/,
  roleArn: /^arn:aws:iam::\d{12}:role\/[A-Za-z0-9+=,.@_-]{1,64}$/,
  region: /^[a-z]{2}(?:-[a-z]+)+-\d$/,
  commit: /^[0-9a-f]{40}$/,
};

/** The options of a command line: every flag exactly once, each value of its shape. Throws one message naming the problem. */
export function parseArgs(argv: readonly string[]): Options {
  const got = new Map<keyof Options, string>();
  for (let i = 0; i < argv.length; i += 2) {
    const flag = argv[i];
    const key = FLAGS.get(flag);
    if (key === undefined) throw new Error(`unknown argument ${flag}`);
    if (i + 1 >= argv.length) throw new Error(`${flag} needs a value`);
    if (got.has(key)) throw new Error(`${flag} is given twice`);
    got.set(key, argv[i + 1]);
  }
  const out = {} as Options;
  for (const [flag, key] of FLAGS) {
    const value = got.get(key);
    if (value === undefined) throw new Error(`${flag} is required`);
    if (!SHAPES[key].test(value)) throw new Error(`${flag} is empty or malformed (an unset repository variable gives an empty value)`);
    out[key] = value;
  }
  return out;
}

export const IMMUTABLE = 'public, max-age=31536000, immutable';
export const PAGE = 'public, max-age=60, s-maxage=300';

/** The `aws` commands, in the order they run. `versionFile` holds the commit. */
export function plan(o: Options, versionFile: string): string[][] {
  const target = `s3://${o.bucket}/`;
  // A filter list that lets one file kind through; releases/ is excluded last, and the last matching filter wins.
  const only = (include: string, type: string, cache: string): string[] => [
    's3', 'sync', o.dist, target,
    '--exclude', '*', '--include', include, '--exclude', 'releases/*',
    '--content-type', type, '--cache-control', cache,
  ];
  return [
    only('assets/*.js', 'text/javascript', IMMUTABLE),
    only('assets/*.css', 'text/css', IMMUTABLE),
    only('*.html', 'text/html; charset=utf-8', PAGE),
    ['s3', 'cp', versionFile, `${target}version.txt`, '--content-type', 'text/plain', '--cache-control', 'no-cache'],
    ['s3', 'sync', o.dist, target, '--delete', '--exclude', 'releases/*', '--exclude', 'version.txt'],
    ['cloudfront', 'create-invalidation', '--distribution-id', o.distributionId, '--paths', '/*'],
  ];
}

/** Everything that touches the world, so the tests can stand in for it. */
export interface Deps {
  /** The job's OIDC token for audience sts.amazonaws.com. */
  token(): Promise<string>;
  /** Writes text to a new file that only this user can read, and returns its path. */
  writeSecret(text: string): string;
  /** Removes a file written by writeSecret. */
  removeSecret(path: string): void;
  /** Writes the commit to a file for version.txt and returns its path. */
  writeVersion(commit: string): string;
  /** Runs `aws` with exactly this environment and returns its standard output. Throws when it fails. */
  run(argv: readonly string[], env: Readonly<Record<string, string>>): string;
  log(line: string): void;
}

/** The least a built site has: its seven pages and ten files today. A dist/ below either is not a site, and the sweep deletes. */
export const MIN_HTML_PAGES = 5;
export const MIN_FILES = 8;
/** The only file types a built site holds (check-dist.ts enforces it); the syncs above give exactly these a content type and a cache rule. */
const TYPES = ['.html', '.js', '.css'];

function filesUnder(dir: string): string[] {
  const out: string[] = [];
  const walk = (d: string): void => {
    for (const f of readdirSync(d)) {
      const p = join(d, f);
      if (statSync(p).isDirectory()) walk(p);
      else out.push(relative(dir, p).split(sep).join('/'));
    }
  };
  walk(dir);
  return out.sort();
}

/** A built site: the pages the router and the error responses name, the assets, at least the floor of pages and files, and no other file type. */
export function checkDist(dist: string): void {
  for (const f of ['index.html', '404.html']) {
    if (!existsSync(join(dist, f))) throw new Error(`${dist} is not a built site: ${f} is missing`);
  }
  if (!existsSync(join(dist, 'assets')) || !statSync(join(dist, 'assets')).isDirectory()) {
    throw new Error(`${dist} is not a built site: assets/ is missing`);
  }
  const files = filesUnder(dist);
  const html = files.filter((f) => f.endsWith('.html')).length;
  if (html < MIN_HTML_PAGES) throw new Error(`${dist} is not a built site: only ${html} html pages (at least ${MIN_HTML_PAGES}); the sweep would empty the bucket`);
  if (files.length < MIN_FILES) throw new Error(`${dist} is not a built site: only ${files.length} files (at least ${MIN_FILES}); the sweep would empty the bucket`);
  const odd = files.find((f) => !TYPES.some((t) => f.endsWith(t)));
  if (odd !== undefined) throw new Error(`${dist} is not a built site: ${odd} is a file type the sync would upload without a content type or cache rule`);
}

interface Credentials {
  AccessKeyId: string;
  SecretAccessKey: string;
  SessionToken: string;
}

function credentialsOf(text: string): Credentials {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error('sts answered something that is not JSON');
  }
  const o = (parsed ?? {}) as Record<string, unknown>;
  for (const k of ['AccessKeyId', 'SecretAccessKey', 'SessionToken']) {
    if (typeof o[k] !== 'string' || o[k] === '') throw new Error(`sts gave no ${k}`);
  }
  return o as unknown as Credentials;
}

/** Masks, assumes the role with the job's token, and runs the plan. Any failure stops everything after it. */
export async function publish(o: Options, d: Deps): Promise<void> {
  checkDist(o.dist);
  // The account id is inside the role ARN and the bucket name: mask it, and each name, before any output exists.
  const account = /^arn:aws:iam::(\d{12}):/.exec(o.roleArn)?.[1] ?? '';
  for (const v of [o.roleArn, o.bucket, o.distributionId, account]) if (v !== '') d.log(`::add-mask::${v}`);

  const jwt = (await d.token()).trim();
  if (!/^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/.test(jwt)) throw new Error('the token service gave no JWT');
  d.log(`::add-mask::${jwt}`);

  const base = { AWS_DEFAULT_REGION: o.region };
  // The token file lives only for the exchange.
  const tokenFile = d.writeSecret(jwt);
  let answer: string;
  try {
    answer = d.run(
      [
        'sts', 'assume-role-with-web-identity',
        '--role-arn', o.roleArn,
        '--role-session-name', `tierstore-site-${o.commit.slice(0, 12)}`,
        '--web-identity-token', `file://${tokenFile}`,
        '--duration-seconds', '3600',
        '--query', 'Credentials', '--output', 'json',
      ],
      base,
    );
  } finally {
    d.removeSecret(tokenFile);
  }
  const c = credentialsOf(answer);
  for (const v of [c.AccessKeyId, c.SecretAccessKey, c.SessionToken]) d.log(`::add-mask::${v}`);
  const env = { ...base, AWS_ACCESS_KEY_ID: c.AccessKeyId, AWS_SECRET_ACCESS_KEY: c.SecretAccessKey, AWS_SESSION_TOKEN: c.SessionToken };

  for (const argv of plan(o, d.writeVersion(o.commit))) {
    d.log(`+ aws ${argv.slice(0, 2).join(' ')}`);
    const out = d.run(argv, env);
    if (out !== '') d.log(out.trimEnd());
  }
}

async function requestToken(): Promise<string> {
  const url = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
  const key = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
  if (!url || !key) throw new Error('the runner offers no OIDC token: the job needs `permissions: id-token: write`');
  const res = await fetch(`${url}&audience=sts.amazonaws.com`, { headers: { Authorization: `Bearer ${key}` } });
  if (!res.ok) throw new Error(`the token service answered ${res.status}`);
  const body = (await res.json()) as { value?: unknown };
  if (typeof body.value !== 'string') throw new Error('the token service gave no value');
  return body.value;
}

function realDeps(): Deps {
  const dir = mkdtempSync(join(process.env.RUNNER_TEMP ?? tmpdir(), 'tierstore-site-'));
  let n = 0;
  const write = (text: string, mode: number): string => {
    const path = join(dir, `file-${n++}`);
    writeFileSync(path, text, { mode });
    return path;
  };
  return {
    token: requestToken,
    writeSecret: (text) => write(text, 0o600),
    removeSecret: (path) => rmSync(path, { force: true }),
    writeVersion: (commit) => write(`${commit}\n`, 0o600),
    run: (argv, env) =>
      execFileSync('aws', [...argv], {
        // Nothing of the job's own environment reaches `aws` but the way to find it and a home for its cache.
        env: { PATH: process.env.PATH ?? '', HOME: process.env.HOME ?? dir, AWS_PAGER: '', AWS_EC2_METADATA_DISABLED: 'true', ...env },
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'inherit'],
        maxBuffer: 64 * 1024 * 1024,
      }),
    log: (line) => process.stdout.write(`${line}\n`),
  };
}

// Run as `node .../publish.ts` (or the compiled publish.js), not when a test imports it.
if (/(^|[\\/])publish\.(ts|js)$/.test(process.argv[1] ?? '')) {
  (async () => publish(parseArgs(process.argv.slice(2)), realDeps()))().catch((e: unknown) => {
    process.stderr.write(`publish: ${e instanceof Error ? e.message : String(e)}\n`);
    process.exitCode = 1;
  });
}
