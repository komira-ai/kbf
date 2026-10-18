// The held steps as a small CLI. Every step is a dry run unless --apply is given; there is no --force; every AWS step
// refuses unless the profile's account equals --account. All tool access goes through `Tools`, so the tests drive the
// steps with a fake and can see exactly which commands ran.

export interface Tools {
  /** Run a read-only command and return its stdout. Throws on a non-zero exit. */
  capture(cmd: string, args: string[]): string;
  /** Run a command with its output shown (diff, install, tests, deploy). */
  stream(cmd: string, args: string[]): void;
  log(line: string): void;
}

export class Refused extends Error {}

export type Command = 'github' | 'dns' | 'site' | 'vars';

export interface Options {
  command: Command;
  apply: boolean;
  account?: string;
  profile?: string;
  repo?: string;
  domain: string;
  oidcProviderArn?: string;
}

const ALLOWED: Record<Command, string[]> = {
  github: ['--repo'],
  dns: ['--account', '--profile', '--domain'],
  site: ['--account', '--profile', '--repo', '--domain', '--oidc-provider-arn'],
  vars: ['--account', '--profile', '--repo'],
};
const REQUIRED: Record<Command, string[]> = {
  github: ['--repo'],
  dns: ['--account', '--profile'],
  site: ['--account', '--profile', '--repo'],
  vars: ['--account', '--profile', '--repo'],
};

export const USAGE = `usage: npm run held -- <command> [flags]
  github --repo <owner/name> [--apply]
  dns    --account <12 digits> --profile <aws profile> [--domain <name>] [--apply]
  site   --account <12 digits> --profile <aws profile> --repo <owner/name> [--domain <name>] [--oidc-provider-arn <arn>] [--apply]
  vars   --account <12 digits> --profile <aws profile> --repo <owner/name>      (read-only; prints commands)
Dry run unless --apply. There is no --force.`;

export function parse(argv: string[]): Options {
  const [command, ...rest] = argv;
  if (!command || !(command in ALLOWED)) throw new Refused(`unknown or missing command\n${USAGE}`);
  const cmd = command as Command;
  const o: Options = { command: cmd, apply: false, domain: 'kbf.dev' };
  const seen = new Set<string>();
  for (let i = 0; i < rest.length; i++) {
    const f = rest[i];
    if (f === '--apply') {
      o.apply = true;
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
  }
  for (const r of REQUIRED[cmd]) if (!seen.has(r)) throw new Refused(`${r} is required`);
  if (o.account !== undefined && !/^\d{12}$/.test(o.account)) throw new Refused('--account must be 12 digits');
  if (o.repo !== undefined && !/^[\w.-]+\/[\w.-]+$/.test(o.repo)) throw new Refused('--repo must be <owner>/<name>');
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

/** Read-only `gh api repos/<repo>`: the immutable numeric ids the OIDC subject needs. */
export function repoIds(t: Tools, repo: string): { repoId: string; ownerId: string } {
  const j = JSON.parse(t.capture('gh', ['api', `repos/${repo}`]));
  return { repoId: String(j.id), ownerId: String(j.owner.id) };
}

export const OIDC_KEYS = ['repo', 'context', 'job_workflow_ref'];

export function github(t: Tools, o: Options): void {
  const repo = o.repo!;
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
  const zones = JSON.parse(
    t.capture('aws', aws(o, 'route53', 'list-hosted-zones-by-name', '--dns-name', o.domain, '--max-items', '1', '--output', 'json')),
  ).HostedZones as { Name: string }[];
  if (!zones.some((z) => z.Name === `${o.domain}.`)) throw new Refused(`no ${o.domain} hosted zone in this account; run dns --apply first`);
  const { repoId, ownerId } = repoIds(t, o.repo!);
  prepare(t);
  const ctx = [
    '-c', `account=${o.account}`, '-c', `domain=${o.domain}`, '-c', `githubRepo=${o.repo}`,
    '-c', `githubRepoId=${repoId}`, '-c', `githubOwnerId=${ownerId}`, '-c', `oidcProviderArn=${arn}`,
  ];
  if (o.apply) {
    t.stream('npx', ['cdk', 'deploy', 'kbf-prod-site', '--profile', o.profile!, '--require-approval', 'broadening', ...ctx]);
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
  for (const [k, v] of rows) t.log(`gh variable set ${k} --env prod --repo ${o.repo} --body '${v}'`);
  t.log(`# Region is always ${REGION}.`);
}

export function main(argv: string[], t: Tools): void {
  const o = parse(argv);
  ({ github, dns, site, vars })[o.command](t, o);
}
