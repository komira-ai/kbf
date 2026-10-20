// The held steps as a small CLI. Every step is a dry run unless --apply is given; there is no --force; every AWS step
// refuses unless the profile's account equals --account. All tool access goes through `Tools`, so the tests drive the
// steps with a fake and can see exactly which commands ran.

export interface Tools {
  /** Run a read-only command and return its stdout. Throws on a non-zero exit. */
  capture(cmd: string, args: string[], cwd?: string): string;
  /** Run a command with its output shown (diff, install, tests, deploy). */
  stream(cmd: string, args: string[], cwd?: string): void;
  log(line: string): void;
  // The members below serve `upload` only (temporary: removed by site-publish.yml).
  /** A new empty scratch directory outside the repository. */
  mkScratch(): string;
  /** Delete a directory made by mkScratch. */
  removeScratch(dir: string): void;
  readFile(path: string): string;
  writeFile(path: string, data: string): void;
  /** Relative paths ('/'-separated) of every file under dir, sorted. */
  listFiles(dir: string): string[];
}

export class Refused extends Error {}

export type Command = 'github' | 'dns' | 'site' | 'vars' | 'upload';

export interface Options {
  command: Command;
  apply: boolean;
  account?: string;
  profile?: string;
  repo?: string;
  domain: string;
  oidcProviderArn?: string;
  /** `site` only: deploy without the domain (no certificate, alias or zone); see README "Before DNS". */
  noDomain: boolean;
  /** `upload` only: the 40-hex commit whose site/ is built and uploaded. */
  commit?: string;
}

const ALLOWED: Record<Command, string[]> = {
  github: ['--repo'],
  dns: ['--account', '--profile', '--domain'],
  site: ['--account', '--profile', '--repo', '--domain', '--oidc-provider-arn', '--no-domain'],
  vars: ['--account', '--profile', '--repo'],
  upload: ['--account', '--profile', '--commit'], // temporary: removed by site-publish.yml
};
const REQUIRED: Record<Command, string[]> = {
  github: ['--repo'],
  dns: ['--account', '--profile'],
  site: ['--account', '--profile', '--repo'],
  vars: ['--account', '--profile', '--repo'],
  upload: ['--account', '--profile', '--commit'],
};

export const USAGE = `usage: npm run held -- <command> [flags]
  github --repo <owner/name> [--apply]
  dns    --account <12 digits> --profile <aws profile> [--domain <name>] [--apply]
  site   --account <12 digits> --profile <aws profile> --repo <owner/name> [--domain <name>] [--oidc-provider-arn <arn>] [--no-domain] [--apply]
  vars   --account <12 digits> --profile <aws profile> --repo <owner/name>      (read-only; prints commands)
  upload --account <12 digits> --profile <aws profile> --commit <40-hex sha> [--apply]   (temporary: builds site/ at that commit and uploads it)
Dry run unless --apply. There is no --force.`;

export function parse(argv: string[]): Options {
  const [command, ...rest] = argv;
  if (!command || !(command in ALLOWED)) throw new Refused(`unknown or missing command\n${USAGE}`);
  const cmd = command as Command;
  const o: Options = { command: cmd, apply: false, domain: 'kbf.dev', noDomain: false };
  const seen = new Set<string>();
  for (let i = 0; i < rest.length; i++) {
    const f = rest[i];
    if (f === '--apply') {
      o.apply = true;
      continue;
    }
    if (f === '--no-domain' && ALLOWED[cmd].includes(f)) {
      o.noDomain = true;
      continue;
    }
    if (!ALLOWED[cmd].includes(f)) throw new Refused(`unknown flag for ${cmd}: ${f}\n${USAGE}`);
    const v = rest[++i];
    if (v === undefined || v.startsWith('--')) throw new Refused(`${f} needs a value`);
    seen.add(f);
    if (f === '--account') o.account = v;
    if (f === '--profile') o.profile = v;
    if (f === '--repo') o.repo = v;
    if (f === '--domain') o.domain = v;
    if (f === '--oidc-provider-arn') o.oidcProviderArn = v;
    if (f === '--commit') o.commit = v;
  }
  for (const r of REQUIRED[cmd]) if (!seen.has(r)) throw new Refused(`${r} is required`);
  if (o.account !== undefined && !/^\d{12}$/.test(o.account)) throw new Refused('--account must be 12 digits');
  if (o.repo !== undefined && !/^[\w.-]+\/[\w.-]+$/.test(o.repo)) throw new Refused('--repo must be <owner>/<name>');
  if (o.commit !== undefined && !/^[0-9a-f]{40}$/.test(o.commit)) throw new Refused('--commit must be a full 40-hex commit sha');
  if (cmd === 'vars' && o.apply) throw new Refused('vars has no --apply: it only prints');
  return o;
}

const REGION = 'us-east-1';
const aws = (o: Options, ...a: string[]) => [...a, '--profile', o.profile!];

/** Read-only: refuse unless the credentials belong to the account the operator named. */
export function checkCaller(t: Tools, o: Options): void {
  const got = t.capture('aws', aws(o, 'sts', 'get-caller-identity', '--query', 'Account', '--output', 'text')).trim();
  if (got !== o.account) throw new Refused(`profile '${o.profile}' is account ${got}, not the --account you passed`);
}

/** Local only: install the pinned dependencies and run the tests before any cdk call. */
function prepare(t: Tools): void {
  t.stream('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund']);
  t.stream('npm', ['test']);
}

/**
 * Read-only `gh api repos/<repo>`: the immutable numeric ids the OIDC subject needs. Refuses unless GitHub's `full_name`
 * equals the --repo given (case-insensitive), so a typo that names a real repository of another owner cannot yield a
 * role that trusts it, and prints all three values so the operator sees what the role will trust.
 */
export function repoIds(t: Tools, repo: string): { repoId: string; ownerId: string; fullName: string } {
  const j = JSON.parse(t.capture('gh', ['api', `repos/${repo}`]));
  const fullName = String(j.full_name ?? '');
  if (fullName.toLowerCase() !== repo.toLowerCase()) {
    throw new Refused(`--repo ${repo} resolves to '${fullName}' on GitHub; pass the exact owner/name (nothing was changed)`);
  }
  const ids = { repoId: String(j.id), ownerId: String(j.owner.id), fullName };
  t.log(`# repository ${fullName}: repo id ${ids.repoId}, owner id ${ids.ownerId} (the role will trust exactly this)`);
  return ids;
}

/** Quote a value for a POSIX shell word: single quotes, each embedded quote written as '\''. */
export const shellQuote = (v: string) => `'${v.replace(/'/g, "'\\''")}'`;

export const OIDC_KEYS = ['repo', 'context', 'job_workflow_ref'];

export function github(t: Tools, o: Options): void {
  const repo = o.repo!;
  repoIds(t, repo); // read-only; refuses a --repo that is not the repository's exact name
  const act = (args: string[]) => {
    if (o.apply) {
      t.log(`+ gh ${args.join(' ')}`);
      t.stream('gh', args);
    } else t.log(`DRY-RUN: gh ${args.join(' ')}`);
  };
  if (!o.apply) t.log('# dry run: nothing is executed. Re-run with --apply to act.');
  act([
    'api', '-X', 'PUT', `repos/${repo}/environments/prod`,
    '-F', 'deployment_branch_policy[protected_branches]=false',
    '-F', 'deployment_branch_policy[custom_branch_policies]=true',
  ]);
  let has = false;
  if (o.apply) {
    const j = JSON.parse(t.capture('gh', ['api', `repos/${repo}/environments/prod/deployment-branch-policies`]));
    has = (j.branch_policies ?? []).some((p: any) => p.name === 'main' && p.type === 'branch');
  }
  if (has) t.log('# policy main/branch already present');
  else act(['api', '-X', 'POST', `repos/${repo}/environments/prod/deployment-branch-policies`, '-f', 'name=main', '-f', 'type=branch']);
  act([
    'api', '-X', 'PUT', `repos/${repo}/actions/oidc/customization/sub`,
    '-F', 'use_default=false', '-F', 'use_immutable_subject=true',
    ...OIDC_KEYS.flatMap((k) => ['-f', `include_claim_keys[]=${k}`]),
  ]);
  t.log('# REMINDER (not run): before relying on the role, run a manual workflow from site-publish.yml@refs/heads/main in');
  t.log("# environment prod that prints the token's sub and job_workflow_ref (never the token), and compare the sub with the");
  t.log('# one in infra/site/README.md. vars checks the GitHub settings, not the token.');
}

export function dns(t: Tools, o: Options): void {
  checkCaller(t, o);
  prepare(t);
  const ctx = ['-c', `account=${o.account}`, '-c', `domain=${o.domain}`];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', 'kbf-dns-zone', '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const out = JSON.parse(
      t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', 'kbf-dns-zone', '--query', 'Stacks[0].Outputs', '--output', 'json')),
    );
    const ns = out.find((x: any) => x.OutputKey === 'NameServers')?.OutputValue as string;
    t.log(`# Name servers of ${o.domain} (copy these four, one per line, no trailing dots, to your registrar):`);
    for (const n of ns.split(',')) t.log(n);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to create the zone.');
    t.stream('npx', ['cdk', 'diff', 'kbf-dns-zone', '--profile', o.profile!, ...ctx]);
  }
}

export function site(t: Tools, o: Options): void {
  checkCaller(t, o);
  const arn = o.oidcProviderArn ?? `arn:aws:iam::${o.account}:oidc-provider/token.actions.githubusercontent.com`;
  try {
    t.capture('aws', aws(o, 'iam', 'get-open-id-connect-provider', '--open-id-connect-provider-arn', arn));
  } catch {
    throw new Refused(`no GitHub OIDC provider at ${arn}; create one first (it is imported, never created here) or pass --oidc-provider-arn`);
  }
  if (o.noDomain) {
    t.log('# no-domain mode: no certificate, alias or Route 53 records; the zone is not looked up');
  } else {
    const zones = JSON.parse(
      t.capture('aws', aws(o, 'route53', 'list-hosted-zones-by-name', '--dns-name', o.domain, '--max-items', '1', '--output', 'json')),
    ).HostedZones as { Name: string }[];
    if (!zones.some((z) => z.Name === `${o.domain}.`)) throw new Refused(`no ${o.domain} hosted zone in this account; run dns --apply first`);
  }
  const { repoId, ownerId, fullName } = repoIds(t, o.repo!);
  prepare(t);
  const ctx = [
    '-c', `account=${o.account}`, '-c', `domain=${o.domain}`, '-c', `githubRepo=${fullName}`, // GitHub's own spelling: the sub claim uses it
    '-c', `githubRepoId=${repoId}`, '-c', `githubOwnerId=${ownerId}`, '-c', `oidcProviderArn=${arn}`,
    ...(o.noDomain ? ['-c', 'noDomain=true'] : []),
  ];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', 'kbf-prod-site', '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const outs = JSON.parse(
      t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', 'kbf-prod-site', '--query', 'Stacks[0].Outputs', '--output', 'json')),
    ) as { OutputKey: string; OutputValue: string }[];
    const host = outs.find((x) => x.OutputKey === 'DistributionDomainName')?.OutputValue;
    if (host) t.log(`# The site is served at https://${host}/`);
    t.log(`# Done. Next: npm run held -- vars --account ${o.account} --profile ${o.profile} --repo ${o.repo}`);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to deploy.');
    t.stream('npx', ['cdk', 'diff', 'kbf-prod-site', '--profile', o.profile!, ...ctx]);
  }
}

/** Read-only GitHub checks. Returns the mismatches; empty when the settings are what `github` sets. */
export function checkGithub(t: Tools, repo: string): string[] {
  const bad: string[] = [];
  const get = (path: string): any => {
    try {
      return JSON.parse(t.capture('gh', ['api', `repos/${repo}/${path}`]));
    } catch {
      return undefined;
    }
  };
  const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
  const env = get('environments/prod');
  const dbp = env?.deployment_branch_policy;
  if (!dbp || dbp.protected_branches !== false || dbp.custom_branch_policies !== true) {
    bad.push('environment prod is missing or does not use custom branch policies only');
  }
  const pols = get('environments/prod/deployment-branch-policies')?.branch_policies;
  if (!Array.isArray(pols) || !same(pols.map((p: any) => [p.name, p.type]), [['main', 'branch']])) {
    bad.push('environment prod does not deploy from exactly the branch main');
  }
  const tmpl = get('actions/oidc/customization/sub');
  if (!tmpl || tmpl.use_default !== false || tmpl.use_immutable_subject !== true || !same(tmpl.include_claim_keys, OIDC_KEYS)) {
    bad.push('OIDC sub template is not repo, context, job_workflow_ref with the immutable subject');
  }
  return bad;
}

export function vars(t: Tools, o: Options): void {
  checkCaller(t, o);
  const bad = checkGithub(t, o.repo!);
  if (bad.length > 0) {
    throw new Refused(`GitHub settings differ from \`github --apply\`:\n- ${bad.join('\n- ')}\nfix them before setting variables`);
  }
  const out = JSON.parse(
    t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', 'kbf-prod-site', '--query', 'Stacks[0].Outputs', '--output', 'json')),
  ) as { OutputKey: string; OutputValue: string }[];
  const val = (k: string) => out.find((x) => x.OutputKey === k)?.OutputValue;
  const rows: [string, string | undefined][] = [
    ['KBF_SITE_BUCKET', val('BucketName')],
    ['KBF_SITE_DISTRIBUTION_ID', val('DistributionId')],
    ['KBF_SITE_PUBLISH_ROLE_ARN', val('PublishRoleArn')],
  ];
  if (rows.some(([, v]) => !v)) throw new Refused('stack kbf-prod-site has no outputs; run site --apply first');
  t.log(`# Environment prod of ${o.repo} is as expected. Run:`);
  for (const [k, v] of rows) t.log(`gh variable set ${k} --env prod --repo ${o.repo} --body ${shellQuote(v!)}`);
  const host = val('DistributionDomainName');
  if (host) t.log(`# The distribution answers at https://${host}/`);
  t.log(`# Region is always ${REGION}.`);
}

// temporary: removed by site-publish.yml
// Uploads the website built from a COMMITTED sha, with the operator's own profile (never the CI role), until the publish
// workflow exists. Order: build and check-dist, then the sync passes, then version.txt, then the invalidation.
export const UPLOAD_CACHE = {
  immutable: 'public, max-age=31536000, immutable',
  page: 'public, max-age=60, s-maxage=300',
  version: 'no-cache',
};

export function upload(t: Tools, o: Options): void {
  checkCaller(t, o);
  const sha = o.commit!;
  let kind = '';
  try {
    kind = t.capture('git', ['cat-file', '-t', sha]).trim();
  } catch {
    /* not an object */
  }
  if (kind !== 'commit') throw new Refused(`${sha} is not a commit of this repository (fetch it first); nothing was built`);
  const outs = JSON.parse(
    t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', 'kbf-prod-site', '--query', 'Stacks[0].Outputs', '--output', 'json')),
  ) as { OutputKey: string; OutputValue: string }[];
  const bucket = outs.find((x) => x.OutputKey === 'BucketName')?.OutputValue;
  const dist = outs.find((x) => x.OutputKey === 'DistributionId')?.OutputValue;
  if (!bucket || !dist) throw new Refused('stack kbf-prod-site has no outputs; run site --apply first');

  const scratch = t.mkScratch();
  const wt = `${scratch}/wt`;
  let added = false;
  try {
    t.stream('git', ['worktree', 'add', '--detach', wt, sha]);
    added = true;
    const site = `${wt}/site`;
    const want = t.readFile(`${site}/.node-version`).trim();
    const have = t.capture('node', ['--version'], site).trim().replace(/^v/, '');
    if (have !== want) throw new Refused(`site/.node-version at ${sha} is ${want}, this node is ${have}; use exactly ${want} (nothing was built)`);
    t.stream('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund'], site);
    t.stream('npm', ['run', 'build'], site);
    try {
      t.stream('node', ['-e', "require('./build/tools/tools/check-dist.js').checkDist('dist')"], site); // check-dist: CSP, links, hygiene
    } catch {
      throw new Refused('check-dist failed on the built dist; nothing was uploaded');
    }
    const dir = `${site}/dist`;
    const files = t.listFiles(dir);
    t.writeFile(`${scratch}/version.txt`, `${sha}\n`);

    const target = `s3://${bucket}/`;
    const dry = o.apply ? [] : ['--dryrun'];
    const notReleases = ['--exclude', 'releases/*']; // last, so it wins: the sync never touches releases/
    const pass = (include: string, type: string, cache: string) => [
      's3', 'sync', dir, target, '--delete', '--exclude', '*', '--include', include, ...notReleases,
      '--content-type', type, '--cache-control', cache, ...dry,
    ];
    const syncs = [
      pass('assets/*.js', 'text/javascript', UPLOAD_CACHE.immutable),
      pass('assets/*.css', 'text/css', UPLOAD_CACHE.immutable),
      pass('*.html', 'text/html; charset=utf-8', UPLOAD_CACHE.page),
      // Anything else in the bucket that the build no longer has goes too; files already uploaded above are skipped.
      ['s3', 'sync', dir, target, '--delete', ...notReleases, '--exclude', 'version.txt', ...dry],
    ];
    const cp = ['s3', 'cp', `${scratch}/version.txt`, `${target}version.txt`, '--content-type', 'text/plain', '--cache-control', UPLOAD_CACHE.version, ...dry];
    const invalidate = ['cloudfront', 'create-invalidation', '--distribution-id', dist, '--paths', '/*'];

    t.log(`# commit ${sha}: ${files.length} files built and checked; they go to s3://${bucket}/ (never releases/):`);
    for (const f of files) t.log(`#   ${f}  ${f.startsWith('assets/') ? 'immutable' : 'page'}`);
    t.log(`#   version.txt  (the commit sha)`);
    if (!o.apply) t.log('# dry run: aws s3 --dryrun lists the uploads and deletes below; nothing is written. Re-run with --apply.');
    for (const a of syncs) {
      t.log(`${o.apply ? '+' : 'DRY-RUN:'} aws ${[...a, '--profile', o.profile!].join(' ')}`);
      t.stream('aws', aws(o, ...a));
    }
    t.log(`${o.apply ? '+' : 'DRY-RUN:'} aws ${[...cp, '--profile', o.profile!].join(' ')}`);
    t.stream('aws', aws(o, ...cp));
    if (o.apply) {
      t.log(`+ aws ${[...invalidate, '--profile', o.profile!].join(' ')}`);
      t.stream('aws', aws(o, ...invalidate));
    } else t.log(`DRY-RUN: aws ${[...invalidate, '--profile', o.profile!].join(' ')}`);
  } finally {
    if (added) t.stream('git', ['worktree', 'remove', '--force', wt]);
    t.removeScratch(scratch);
  }
}

export function main(argv: string[], t: Tools): void {
  const o = parse(argv);
  ({ github, dns, site, vars, upload })[o.command](t, o);
}
