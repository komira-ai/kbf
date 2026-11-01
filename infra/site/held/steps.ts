// The held steps as a small CLI. Every step is a dry run unless --apply is given; there is no --force; every AWS step
// refuses unless the profile's account equals --account. All tool access goes through `Tools`, so the tests drive the
// steps with a fake and can see exactly which commands ran.

import { SiteNames, oidcSubject, siteId, siteNames } from '../lib/config';

export interface Tools {
  /** Run a read-only command and return its stdout. Throws on a non-zero exit. */
  capture(cmd: string, args: string[], cwd?: string): string;
  /** Run a command with its output shown (diff, install, tests, deploy); `cwd` defaults to the infra/site root, `input` is written to its stdin. */
  stream(cmd: string, args: string[], opts?: { cwd?: string; input?: string }): void;
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

export type Command = 'github' | 'oidc-provider' | 'dns' | 'site' | 'vars' | 'upload';

export interface Options {
  command: Command;
  apply: boolean;
  account?: string;
  profile?: string;
  repo?: string;
  reviewer?: string;
  domain: string;
  /** Explicit site id; absent for a domain SITES knows (then the id comes from the domain). */
  site?: string;
  oidcProviderArn?: string;
  /** `site` only: deploy without the domain (no certificate, alias or zone); see README "Before DNS". */
  noDomain: boolean;
  /** `upload` only: the 40-hex commit whose site/ is built and uploaded. */
  commit?: string;
}

const ALLOWED: Record<Command, string[]> = {
  github: ['--repo', '--reviewer', '--domain', '--site'],
  'oidc-provider': ['--account', '--profile'],
  dns: ['--account', '--profile', '--domain', '--site'],
  site: ['--account', '--profile', '--repo', '--domain', '--site', '--oidc-provider-arn', '--no-domain'],
  vars: ['--account', '--profile', '--repo', '--domain', '--site'],
  upload: ['--account', '--profile', '--commit', '--domain', '--site'], // temporary: removed by site-publish.yml
};
const REQUIRED: Record<Command, string[]> = {
  github: ['--repo', '--reviewer'],
  'oidc-provider': ['--account', '--profile'],
  dns: ['--account', '--profile'],
  site: ['--account', '--profile', '--repo'],
  vars: ['--account', '--profile', '--repo'],
  upload: ['--account', '--profile', '--commit'],
};

export const USAGE = `usage: npm run held -- <command> [flags]
  github --repo <owner/name> --reviewer <github login of the approver> [--domain <name>] [--site <id>] [--apply]
  oidc-provider --account <12 digits> --profile <aws profile> [--apply]   (creates the GitHub OIDC provider only if absent)
  dns    --account <12 digits> --profile <aws profile> [--domain <name>] [--site <id>] [--apply]
  site   --account <12 digits> --profile <aws profile> --repo <owner/name> [--domain <name>] [--site <id>] [--oidc-provider-arn <arn>] [--no-domain] [--apply]
  vars   --account <12 digits> --profile <aws profile> --repo <owner/name> [--domain <name>] [--site <id>]   (read-only; prints commands)
  upload --account <12 digits> --profile <aws profile> --commit <40-hex sha> [--domain <name>] [--site <id>] [--apply]   (temporary: builds site/ at that commit and uploads it)
--domain picks the site (default kbf.dev; known: kbf.dev, tierstore.dev); every stack, bucket, role and GitHub environment name derives from its id.
--site <id> is for a domain that is not known (it is never given kbf's names by default); it may not contradict a known domain.
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
    if (f === '--reviewer') o.reviewer = v;
    if (f === '--domain') o.domain = v;
    if (f === '--site') o.site = v;
    if (f === '--oidc-provider-arn') o.oidcProviderArn = v;
    if (f === '--commit') o.commit = v;
  }
  for (const r of REQUIRED[cmd]) if (!seen.has(r)) throw new Refused(`${r} is required`);
  if (o.account !== undefined && !/^\d{12}$/.test(o.account)) throw new Refused('--account must be 12 digits');
  if (o.repo !== undefined && !/^[\w.-]+\/[\w.-]+$/.test(o.repo)) throw new Refused('--repo must be <owner>/<name>');
  if (o.commit !== undefined && !/^[0-9a-f]{40}$/.test(o.commit)) throw new Refused('--commit must be a full 40-hex commit sha');
  if (o.reviewer !== undefined && !/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/.test(o.reviewer)) throw new Refused('--reviewer must be a GitHub login');
  try {
    siteId(o.domain, o.site);
  } catch (e) {
    throw new Refused((e as Error).message);
  }
  if (cmd === 'vars' && o.apply) throw new Refused('vars has no --apply: it only prints');
  return o;
}

const REGION = 'us-east-1';
const aws = (o: Options, ...a: string[]) => [...a, '--profile', o.profile!];
/** The names of the site the options select. kbf.dev (the default) has the original names. */
export const namesFor = (o: Pick<Options, 'domain' | 'site'>): SiteNames => siteNames(siteId(o.domain, o.site));
/** `-c site=<id>` only when the operator gave --site; a known domain needs none, so kbf.dev's cdk command lines are unchanged. */
const siteCtx = (o: Options) => (o.site === undefined ? [] : ['-c', `site=${o.site}`]);

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

const subFor = (i: { fullName: string; repoId: string; ownerId: string }, n: SiteNames) => oidcSubject({ repo: i.fullName, repoId: i.repoId, ownerId: i.ownerId }, n);

/** Quote a value for a POSIX shell word: single quotes, each embedded quote written as '\''. */
export const shellQuote = (v: string) => `'${v.replace(/'/g, "'\\''")}'`;

export const OIDC_KEYS = ['repo', 'context', 'job_workflow_ref'];

/**
 * The two GitHub environments of one release, and why there are two (one approval click per release). For kbf.dev they are
 * `prod` and `prod-site`; another site has its own pair (`prod-<id>`, `prod-site-<id>`), so sites in one repository never
 * share a reviewer rule, policy or variable.
 *  - approval (`prod`): the ONLY environment with a required reviewer. The `publish` job (contents: write) references it, so the
 *    reviewer is asked once. Policies: branch `main` (dispatch dry runs) and tag `v*` (the tag-push run).
 *  - site (`prod-site`): NO reviewer. The site job (id-token: write) references it and `needs` the approved publish job, so the
 *    site cannot start before the GitHub release exists and the reviewer is not asked a second time. Policy: tag `v*` only.
 *    The AWS role trusts this environment in its `sub`, so the approved publish job can never assume it.
 */
export interface EnvSpec { name: string; reviewer: boolean; policies: [string, string][] }
export const environmentsFor = (n: SiteNames): EnvSpec[] => [
  { name: n.approvalEnvironment, reviewer: true, policies: [['main', 'branch'], ['v*', 'tag']] },
  { name: n.githubEnvironment, reviewer: false, policies: [['v*', 'tag']] },
];
export const ENVIRONMENTS: EnvSpec[] = environmentsFor(siteNames('kbf'));
/** Kept for the tests: the policies of kbf.dev's approval environment. */
export const POLICIES: [string, string][] = ENVIRONMENTS[0].policies;

/** True when a failed `gh api` call was an HTTP 404 (gh prints "(HTTP 404)" on stderr, which execFileSync puts in the message). */
function isNotFound(e: unknown): boolean {
  const err = e as { message?: unknown; stderr?: unknown };
  return /\(HTTP 404\)/.test(`${String(err?.message ?? '')}\n${String(err?.stderr ?? '')}`);
}

/** Read-only GET of a GitHub API path under the repo; undefined only for HTTP 404 (an environment not yet created). Any other failure aborts. */
function ghGet(t: Tools, repo: string, path: string): any {
  try {
    return JSON.parse(t.capture('gh', ['api', `repos/${repo}/${path}`]));
  } catch (e) {
    if (isNotFound(e)) return undefined;
    throw new Refused(`gh api repos/${repo}/${path} failed, and not with a 404 (permission or network?): ${String((e as Error)?.message ?? e).split('\n')[0]} (nothing was changed)`);
  }
}

/** The required-reviewer rule of an environment as GitHub reports it: reviewer ids and prevent_self_review. */
function reviewerRule(env: any): { ids: number[]; preventSelf: unknown } | undefined {
  const rule = (env?.protection_rules ?? []).find((r: any) => r.type === 'required_reviewers');
  if (!rule) return undefined;
  return { ids: (rule.reviewers ?? []).map((r: any) => r.reviewer?.id), preventSelf: rule.prevent_self_review };
}

/** Read-only: resolve the reviewer login to the numeric id of a user (a team or an organisation cannot approve here). */
export function reviewerId(t: Tools, login: string): number {
  const j = JSON.parse(t.capture('gh', ['api', `users/${login}`]));
  if (j.type !== 'User' || typeof j.id !== 'number') throw new Refused(`--reviewer ${login} is not a GitHub user (nothing was changed)`);
  t.log(`# reviewer ${j.login ?? login}: user id ${j.id}`);
  return j.id;
}

export function github(t: Tools, o: Options): void {
  const repo = o.repo!;
  const names = namesFor(o);
  const ids = repoIds(t, repo); // read-only; refuses a --repo that is not the repository's exact name
  const reviewer = reviewerId(t, o.reviewer!);
  const act = (args: string[], input?: string) => {
    // A body goes to gh's stdin, and gh reads stdin only when told to: `--input -` must be on the argv it runs with,
    // not only on the line shown (without it gh sends no body and GitHub applies its defaults).
    const run = input === undefined ? args : [...args, '--input', '-'];
    const shown = input === undefined ? run : [...run, `<<< ${shellQuote(input)}`];
    if (o.apply) {
      t.log(`+ gh ${shown.join(' ')}`);
      t.stream('gh', run, { input });
    } else t.log(`DRY-RUN: gh ${shown.join(' ')}`);
  };
  if (!o.apply) t.log('# dry run: nothing is executed. Re-run with --apply to act.');
  for (const e of environmentsFor(names)) {
    // PUT replaces the whole protection set, so it always carries every field it manages.
    const body = JSON.stringify({
      reviewers: e.reviewer ? [{ type: 'User', id: reviewer }] : [],
      prevent_self_review: false, // the tag pusher is the approver; with this on, a release would deadlock
      can_admins_bypass: false,
      deployment_branch_policy: { protected_branches: false, custom_branch_policies: true },
    });
    const env = ghGet(t, repo, `environments/${e.name}`);
    const rule = reviewerRule(env);
    const dbp = env?.deployment_branch_policy;
    const reviewersOk = e.reviewer ? rule !== undefined && rule.ids.length === 1 && rule.ids[0] === reviewer && rule.preventSelf === false : rule === undefined || rule.ids.length === 0;
    const envOk = env !== undefined && reviewersOk && env.can_admins_bypass === false && dbp?.protected_branches === false && dbp?.custom_branch_policies === true;
    if (envOk) t.log(`# environment ${e.name} already has ${e.reviewer ? 'this reviewer' : 'no reviewer'}, admin bypass off, custom policies`);
    else act(['api', '-X', 'PUT', `repos/${repo}/environments/${e.name}`], body);
    const pols = ghGet(t, repo, `environments/${e.name}/deployment-branch-policies`)?.branch_policies ?? [];
    for (const [name, type] of e.policies) {
      if (pols.some((p: any) => p.name === name && p.type === type)) t.log(`# ${e.name}: policy ${name}/${type} already present`);
      else act(['api', '-X', 'POST', `repos/${repo}/environments/${e.name}/deployment-branch-policies`, '-f', `name=${name}`, '-f', `type=${type}`]);
    }
    for (const p of pols) {
      if (!e.policies.some(([n, ty]) => n === p.name && ty === p.type)) {
        t.log(`# WARNING: ${e.name} has an extra policy ${p.name}/${p.type}; it is not removed here, delete it by hand (vars refuses while it exists)`);
      }
    }
  }
  act([
    'api', '-X', 'PUT', `repos/${repo}/actions/oidc/customization/sub`,
    '-F', 'use_default=false', '-F', 'use_immutable_subject=true',
    ...OIDC_KEYS.flatMap((k) => ['-f', `include_claim_keys[]=${k}`]),
  ]);
  t.log(`# The role must trust environment ${names.githubEnvironment} (not ${names.approvalEnvironment}); its sub is:`);
  t.log(`#   ${subFor(ids, names)}`);
  t.log('# If the stack was deployed with another environment in its trust, re-run the `site` step (diff, then --apply).');
  t.log(`# REMINDER (not run): before relying on the role, run a manual workflow from ${names.publishWorkflow.split('/').pop()}@refs/heads/main in`);
  t.log(`# environment ${names.githubEnvironment} that prints the token's sub and job_workflow_ref (never the token), and compare the sub with the`);
  t.log('# one above. vars checks the GitHub settings, not the token.');
}

export const OIDC_URL = 'https://token.actions.githubusercontent.com';
export const OIDC_AUDIENCE = 'sts.amazonaws.com';

/** Creates the account's GitHub OIDC provider only when absent (an account holds one per URL); never touches an existing one. */
export function oidcProvider(t: Tools, o: Options): void {
  checkCaller(t, o);
  const list = JSON.parse(t.capture('aws', aws(o, 'iam', 'list-open-id-connect-providers', '--output', 'json'))).OpenIDConnectProviderList as
    | { Arn: string }[]
    | undefined;
  const have = (list ?? []).find((p) => p.Arn.endsWith(':oidc-provider/token.actions.githubusercontent.com'));
  if (have) {
    t.log(`# GitHub OIDC provider already present: ${have.Arn} (nothing to do)`);
    return;
  }
  const create = aws(o, 'iam', 'create-open-id-connect-provider', '--url', OIDC_URL, '--client-id-list', OIDC_AUDIENCE);
  if (o.apply) {
    t.log(`+ aws ${create.join(' ')}`);
    t.stream('aws', create);
  } else {
    t.log('# dry run: nothing is executed. Re-run with --apply to act.');
    t.log(`DRY-RUN: aws ${create.join(' ')}`);
  }
}

export function dns(t: Tools, o: Options): void {
  checkCaller(t, o);
  prepare(t);
  const stack = namesFor(o).zoneStack;
  const ctx = ['-c', `account=${o.account}`, '-c', `domain=${o.domain}`, ...siteCtx(o)];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', stack, '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const out = JSON.parse(
      t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', stack, '--query', 'Stacks[0].Outputs', '--output', 'json')),
    );
    const ns = out.find((x: any) => x.OutputKey === 'NameServers')?.OutputValue as string;
    t.log(`# Name servers of ${o.domain} (copy these four, one per line, no trailing dots, to your registrar):`);
    for (const n of ns.split(',')) t.log(n);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to create the zone.');
    t.stream('npx', ['cdk', 'diff', stack, '--profile', o.profile!, ...ctx]);
  }
}

export function site(t: Tools, o: Options): void {
  checkCaller(t, o);
  const stack = namesFor(o).siteStack;
  const arn = o.oidcProviderArn ?? `arn:aws:iam::${o.account}:oidc-provider/token.actions.githubusercontent.com`;
  try {
    t.capture('aws', aws(o, 'iam', 'get-open-id-connect-provider', '--open-id-connect-provider-arn', arn));
  } catch {
    throw new Refused(`no GitHub OIDC provider at ${arn}; run: npm run held -- oidc-provider --apply (with this account and profile) first (it is imported, never created here) or pass --oidc-provider-arn`);
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
    ...siteCtx(o),
  ];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', stack, '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
    const outs = JSON.parse(
      t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', stack, '--query', 'Stacks[0].Outputs', '--output', 'json')),
    ) as { OutputKey: string; OutputValue: string }[];
    const host = outs.find((x) => x.OutputKey === 'DistributionDomainName')?.OutputValue;
    if (host) t.log(`# The site is served at https://${host}/`);
    t.log(`# Done. Next: npm run held -- vars --account ${o.account} --profile ${o.profile} --repo ${o.repo}`);
  } else {
    t.log('# dry run: cdk diff only. Re-run with --apply to deploy.');
    t.stream('npx', ['cdk', 'diff', stack, '--profile', o.profile!, ...ctx]);
  }
}

/** Read-only GitHub checks. Returns the mismatches; empty when the settings are what `github` sets. */
export function checkGithub(t: Tools, repo: string, names: SiteNames = siteNames('kbf')): string[] {
  const bad: string[] = [];
  const get = (path: string): any => {
    try {
      return JSON.parse(t.capture('gh', ['api', `repos/${repo}/${path}`]));
    } catch {
      return undefined;
    }
  };
  const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
  for (const e of environmentsFor(names)) {
    const env = get(`environments/${e.name}`);
    const dbp = env?.deployment_branch_policy;
    if (!dbp || dbp.protected_branches !== false || dbp.custom_branch_policies !== true) {
      bad.push(`environment ${e.name} is missing or does not use custom branch policies only`);
    }
    const rule = reviewerRule(env);
    if (e.reviewer) {
      if (!rule || rule.ids.length === 0) bad.push(`environment ${e.name} has no required reviewer`);
      else if (rule.preventSelf !== false) bad.push(`environment ${e.name} does not allow the tag pusher to approve (prevent_self_review must be false)`);
    } else if (rule && rule.ids.length > 0) {
      bad.push(`environment ${e.name} has a required reviewer: it would raise a second approval prompt per release (only ${names.approvalEnvironment} has one)`);
    }
    if (env?.can_admins_bypass !== false) bad.push(`environment ${e.name} lets administrators bypass its protection rules`);
    const pols = get(`environments/${e.name}/deployment-branch-policies`)?.branch_policies;
    const want = e.policies.map((p) => p.join('/')).sort();
    if (!Array.isArray(pols) || !same(pols.map((p: any) => `${p.name}/${p.type}`).sort(), want)) {
      bad.push(`environment ${e.name} does not deploy from exactly ${e.policies.map(([n, ty]) => `${ty} ${n}`).join(' and ')}`);
    }
  }
  const tmpl = get('actions/oidc/customization/sub');
  if (!tmpl || tmpl.use_default !== false || tmpl.use_immutable_subject !== true || !same(tmpl.include_claim_keys, OIDC_KEYS)) {
    bad.push('OIDC sub template is not repo, context, job_workflow_ref with the immutable subject');
  }
  return bad;
}

export function vars(t: Tools, o: Options): void {
  checkCaller(t, o);
  const names = namesFor(o);
  const stack = names.siteStack;
  const bad = checkGithub(t, o.repo!, names);
  if (bad.length > 0) {
    throw new Refused(`GitHub settings differ from \`github --apply\`:\n- ${bad.join('\n- ')}\nfix them before setting variables`);
  }
  const out = JSON.parse(
    t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', stack, '--query', 'Stacks[0].Outputs', '--output', 'json')),
  ) as { OutputKey: string; OutputValue: string }[];
  const val = (k: string) => out.find((x) => x.OutputKey === k)?.OutputValue;
  const [vBucket, vDist, vRole] = names.variables;
  const rows: [string, string | undefined][] = [
    [vBucket, val('BucketName')],
    [vDist, val('DistributionId')],
    [vRole, val('PublishRoleArn')],
  ];
  if (rows.some(([, v]) => !v)) throw new Refused(`stack ${stack} has no outputs; run site --apply first`);
  t.log(`# Environments ${environmentsFor(names).map((e) => e.name).join(' and ')} of ${o.repo} are as expected. The variables belong to ${names.githubEnvironment}, the environment of the job that assumes the role. Run:`);
  for (const [k, v] of rows) t.log(`gh variable set ${k} --env ${names.githubEnvironment} --repo ${o.repo} --body ${shellQuote(v!)}`);
  t.log(`# Region is always ${REGION}. The role's trust must name environment ${names.githubEnvironment}; its sub is:`);
  t.log(`#   ${subFor(repoIds(t, o.repo!), names)}`);
  const host = val('DistributionDomainName');
  if (host) t.log(`# The distribution answers at https://${host}/`);
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
  const stack = namesFor(o).siteStack;
  const sha = o.commit!;
  let kind = '';
  try {
    kind = t.capture('git', ['cat-file', '-t', sha]).trim();
  } catch {
    /* not an object */
  }
  if (kind !== 'commit') throw new Refused(`${sha} is not a commit of this repository (fetch it first); nothing was built`);
  const outs = JSON.parse(
    t.capture('aws', aws(o, 'cloudformation', 'describe-stacks', '--region', REGION, '--stack-name', stack, '--query', 'Stacks[0].Outputs', '--output', 'json')),
  ) as { OutputKey: string; OutputValue: string }[];
  const bucket = outs.find((x) => x.OutputKey === 'BucketName')?.OutputValue;
  const dist = outs.find((x) => x.OutputKey === 'DistributionId')?.OutputValue;
  if (!bucket || !dist) throw new Refused(`stack ${stack} has no outputs; run site --apply first`);

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
    t.stream('npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund'], { cwd: site });
    t.stream('npm', ['run', 'build'], { cwd: site });
    try {
      t.stream('node', ['-e', "require('./build/tools/tools/check-dist.js').checkDist('dist')"], { cwd: site }); // check-dist: CSP, links, hygiene
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
  ({ github, 'oidc-provider': oidcProvider, dns, site, vars, upload })[o.command](t, o);
}
