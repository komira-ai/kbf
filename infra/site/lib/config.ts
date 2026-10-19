// Nothing here is a secret, an account id or an organization name: the account, the repository and its numeric ids
// are CDK context (-c account=... -c githubRepo=... -c githubRepoId=... -c githubOwnerId=...), supplied by the
// held CLI (held/main.ts) from read-only `gh api` / `aws sts` calls.

export const REGION = 'us-east-1'; // ACM certificates for CloudFront must live here, so every stack does
export const DEFAULT_DOMAIN = 'kbf.dev';

export const GITHUB_OIDC_HOST = 'token.actions.githubusercontent.com';
export const OIDC_AUDIENCE = 'sts.amazonaws.com';

/** The workflow the role trusts. It is a reusable workflow: job_workflow_ref names the called file, not the caller. */
export const GITHUB_ENVIRONMENT = 'prod';
export const PUBLISH_WORKFLOW = '.github/workflows/site-publish.yml';
export const WORKFLOW_REF = 'refs/heads/main';

export const RELEASES_PREFIX = 'releases/';

export const bucketName = (account: string) => `kbf-prod-site-${account}`;
export const HEADERS_POLICY_NAME = 'kbf-prod-site-headers';
export const ROUTER_NAME = 'kbf-prod-site-router';
export const ROLE_NAME = 'kbf-prod-publish';
export const ZONE_STACK_NAME = 'kbf-dns-zone';
export const SITE_STACK_NAME = 'kbf-prod-site';

export interface ZoneConfig {
  account: string;
  domain: string;
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
  return { account, domain };
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

/** sub = repo:<owner>@<ownerId>/<name>@<repoId>:environment:prod:job_workflow_ref:<repo>/<file>@<ref> */
export function oidcSubject(c: SiteConfig): string {
  const [owner, name] = c.repo.split('/');
  return (
    `repo:${owner}@${c.ownerId}/${name}@${c.repoId}` +
    `:environment:${GITHUB_ENVIRONMENT}:job_workflow_ref:${c.repo}/${PUBLISH_WORKFLOW}@${WORKFLOW_REF}`
  );
}
