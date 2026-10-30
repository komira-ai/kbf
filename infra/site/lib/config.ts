// Nothing here is a secret, an account id or an organization name: the account, the repository and its numeric ids
// are CDK context (-c account=... -c githubRepo=... -c githubRepoId=... -c githubOwnerId=...), supplied by the
// held CLI (held/main.ts) from read-only `gh api` / `aws sts` calls.

export const REGION = 'us-east-1'; // ACM certificates for CloudFront must live here, so every stack does
export const DEFAULT_DOMAIN = 'kbf.dev';

export const GITHUB_OIDC_HOST = 'token.actions.githubusercontent.com';
export const OIDC_AUDIENCE = 'sts.amazonaws.com';

/**
 * The role trusts the site environment (`prod-site` for kbf.dev), NOT the approval environment (`prod`). The approval
 * environment carries the one required reviewer (the release's single approval click, on the `publish` job); the site
 * environment has no reviewer, admits only `v*` tags, and its job `needs` the approved one. Pinning the role to the site
 * environment keeps the approved `publish` job (contents: write) from ever assuming it.
 */
export const WORKFLOW_REF = 'refs/heads/main';
export const RELEASES_PREFIX = 'releases/';

/**
 * Sites hosted by this app: domain -> site id. Every resource name is derived from the id (siteNames), so two sites in one
 * AWS account, and in one GitHub repository, share no name. kbf.dev's names are the ones its stacks were first deployed
 * with: CloudFormation replaces a resource whose logical id or physical name changes, so they must never change.
 * A domain that is not listed needs an explicit id (`--site`); it is never silently given another site's names.
 */
export const SITES: Readonly<Record<string, string>> = { 'kbf.dev': 'kbf', 'tierstore.dev': 'tierstore' };

const SITE_ID = /^[a-z][a-z0-9]{0,15}$/;

/** The id of the site served on `domain`: the explicit one when given (it may not contradict SITES), else the mapped one. */
export function siteId(domain: string, explicit?: string): string {
  const mapped = SITES[domain];
  if (explicit === undefined || explicit === '') {
    if (mapped === undefined) throw new Error(`no site id for domain '${domain}': pass --site <id> (-c site=<id>); known domains: ${Object.keys(SITES).join(', ')}`);
    return mapped;
  }
  if (!SITE_ID.test(explicit)) throw new Error(`bad site id '${explicit}': lower-case letters and digits, starting with a letter, at most 16`);
  if (mapped !== undefined && mapped !== explicit) throw new Error(`domain ${domain} is site '${mapped}', not '${explicit}'`);
  for (const [d, id] of Object.entries(SITES)) {
    if (id === explicit && d !== domain) throw new Error(`site id '${explicit}' belongs to ${d}; ${domain} needs its own id`);
  }
  if (mapped === undefined) {
    // An unlisted id must claim no name a listed site claims (id 'site' would take kbf's `prod-site` environment).
    const mine = new Set(claimedNames(siteNames(explicit)));
    for (const [d, id] of Object.entries(SITES)) {
      const clash = claimedNames(siteNames(id)).find((n) => mine.has(n));
      if (clash !== undefined) throw new Error(`site id '${explicit}' would claim '${clash}', which belongs to ${d}; choose another id`);
    }
  }
  return explicit;
}

/** Everything the site infrastructure names, derived from one id. For `kbf` these are the original, deployed names. */
export interface SiteNames {
  id: string;
  zoneStack: string;
  siteStack: string;
  bucket: (account: string) => string;
  headersPolicy: string;
  router: string;
  role: string;
  /** GitHub environment with the one required reviewer; the release's `publish` job runs in it. */
  approvalEnvironment: string;
  /** GitHub environment the role trusts; the job that assumes it runs in it. No reviewer. */
  githubEnvironment: string;
  /** The reusable workflow (a file under .github/workflows/) the role trusts. */
  publishWorkflow: string;
  /** Prefix of the GitHub variables (`<prefix>_BUCKET` ...). */
  varPrefix: string;
  /** The three variable names `vars` prints: bucket, distribution id, publish role ARN. */
  variables: [string, string, string];
}

export function siteNames(id: string): SiteNames {
  if (!SITE_ID.test(id)) throw new Error(`bad site id '${id}': lower-case letters and digits, starting with a letter, at most 16`);
  const kbf = id === 'kbf';
  const varPrefix = `${id.toUpperCase()}_SITE`;
  return {
    id,
    zoneStack: `${id}-dns-zone`,
    siteStack: `${id}-prod-site`,
    bucket: (account) => `${id}-prod-site-${account}`,
    headersPolicy: `${id}-prod-site-headers`,
    router: `${id}-prod-site-router`,
    role: `${id}-prod-publish`,
    approvalEnvironment: kbf ? 'prod' : `prod-${id}`,
    githubEnvironment: kbf ? 'prod-site' : `prod-site-${id}`,
    publishWorkflow: kbf ? '.github/workflows/site-publish.yml' : `.github/workflows/${id}-site-publish.yml`,
    varPrefix,
    variables: [`${varPrefix}_BUCKET`, `${varPrefix}_DISTRIBUTION_ID`, `${varPrefix}_PUBLISH_ROLE_ARN`],
  };
}

/** Every name a site claims (account-scoped ones for a placeholder account), for collision checks. */
export function claimedNames(n: SiteNames): string[] {
  return [
    n.zoneStack, n.siteStack, n.bucket('000000000000'), n.headersPolicy, n.router, n.role,
    n.approvalEnvironment, n.githubEnvironment, n.publishWorkflow, ...n.variables,
  ];
}

// kbf.dev's names under their original constant names (the pins in test/kbf-names.test.ts use these).
const KBF = siteNames('kbf');
export const APPROVAL_ENVIRONMENT = KBF.approvalEnvironment;
export const GITHUB_ENVIRONMENT = KBF.githubEnvironment;
export const PUBLISH_WORKFLOW = KBF.publishWorkflow;
export const bucketName = KBF.bucket;
export const HEADERS_POLICY_NAME = KBF.headersPolicy;
export const ROUTER_NAME = KBF.router;
export const ROLE_NAME = KBF.role;
export const ZONE_STACK_NAME = KBF.zoneStack;
export const SITE_STACK_NAME = KBF.siteStack;

export interface ZoneConfig {
  account: string;
  domain: string;
  /** Site id; when absent it is the one SITES maps `domain` to. */
  site?: string;
}

export interface SiteConfig extends ZoneConfig {
  /** `owner/name` of the repository whose workflow may assume the role. */
  repo: string;
  /** Immutable numeric ids (`gh api repos/<repo> --jq '.id,.owner.id'`). */
  repoId: string;
  ownerId: string;
}

const ACCOUNT = /^\d{12}$/;
const DOMAIN = /^([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,}$/;
const REPO = /^[\w.-]+\/[\w.-]+$/;
const NUMERIC_ID = /^[1-9]\d*$/;

export function zoneConfig(get: (key: string) => unknown): ZoneConfig {
  const account = String(get('account') ?? '');
  const domain = String(get('domain') ?? DEFAULT_DOMAIN);
  if (!ACCOUNT.test(account)) throw new Error('pass -c account=<12 digits>');
  if (!DOMAIN.test(domain)) throw new Error(`bad domain '${domain}'`);
  const site = siteId(domain, get('site') === undefined ? undefined : String(get('site')));
  return { account, domain, site };
}

/** `-c noDomain=true` reaches CDK as the string 'true'; anything else non-empty must not silently pick domain mode. */
export function noDomain(get: (key: string) => unknown): boolean {
  const v = get('noDomain');
  if (v === undefined || v === null || v === '' || v === false || v === 'false') return false;
  if (v === true || v === 'true') return true;
  throw new Error(`-c noDomain must be true or false, got '${String(v)}'`);
}

export function siteConfig(get: (key: string) => unknown): SiteConfig {
  const base = zoneConfig(get);
  const repo = String(get('githubRepo') ?? '');
  const repoId = String(get('githubRepoId') ?? '');
  const ownerId = String(get('githubOwnerId') ?? '');
  if (!REPO.test(repo)) throw new Error('pass -c githubRepo=<owner>/<name>');
  if (!NUMERIC_ID.test(repoId) || !NUMERIC_ID.test(ownerId)) {
    throw new Error('pass -c githubRepoId=<numeric id> -c githubOwnerId=<numeric id> (gh api repos/<repo> --jq ".id,.owner.id")');
  }
  return { ...base, repo, repoId, ownerId };
}

/** The names of the site `c` is for. */
export const namesOf = (c: Pick<ZoneConfig, 'domain' | 'site'>): SiteNames => siteNames(siteId(c.domain, c.site));

/** sub = repo:<owner>@<ownerId>/<name>@<repoId>:environment:<site environment>:job_workflow_ref:<repo>/<file>@<ref> (kbf.dev: prod-site, site-publish.yml) */
export function oidcSubject(c: Pick<SiteConfig, 'repo' | 'repoId' | 'ownerId'>, names: SiteNames = KBF): string {
  const [owner, name] = c.repo.split('/');
  return (
    `repo:${owner}@${c.ownerId}/${name}@${c.repoId}` +
    `:environment:${names.githubEnvironment}:job_workflow_ref:${c.repo}/${names.publishWorkflow}@${WORKFLOW_REF}`
  );
}
