// Two sites in one AWS account (and one GitHub repository) must share no resource name, stack, role, GitHub environment or
// variable. The names are derived from a site id; kbf.dev's are pinned in kbf-names.test.ts.
import { App } from 'aws-cdk-lib';
import { Template } from 'aws-cdk-lib/assertions';
import * as c from '../lib/config';
import { ZoneStack } from '../lib/dns-stack';
import { SiteStack } from '../lib/site-stack';
import { Refused, Tools, main, parse } from '../held/steps';

const ACCOUNT = '123456789012'; // the AWS documentation placeholder
const REPO = 'example-org/example-repo';
const PROVIDER = `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;
const env = { account: ACCOUNT, region: 'us-east-1' };
const base = { account: ACCOUNT, repo: REPO, repoId: '222', ownerId: '111' };

const KBF = c.siteNames('kbf');
const TIER = c.siteNames('tierstore');

/** Every name a site claims, as strings, for the given account. */
const claimed = (n: c.SiteNames): string[] => [
  n.id, n.zoneStack, n.siteStack, n.bucket(ACCOUNT), n.headersPolicy, n.router, n.role,
  n.approvalEnvironment, n.githubEnvironment, n.publishWorkflow, n.varPrefix,
  ...n.variables,
];

describe('site ids', () => {
  test('kbf.dev is kbf and tierstore.dev is tierstore', () => {
    expect(c.siteId('kbf.dev')).toBe('kbf');
    expect(c.siteId('tierstore.dev')).toBe('tierstore');
  });
  test('an unmapped domain needs an explicit id; it is never silently given kbf names', () => {
    expect(() => c.siteId('docs.example.org')).toThrow(/--site/);
    expect(c.siteId('docs.example.org', 'docs')).toBe('docs');
  });
  test('an explicit id may not contradict the map or take another mapped site\'s id', () => {
    expect(() => c.siteId('tierstore.dev', 'kbf')).toThrow(/tierstore/);
    expect(() => c.siteId('docs.example.org', 'kbf')).toThrow(/kbf/);
    expect(c.siteId('kbf.dev', 'kbf')).toBe('kbf');
  });
  test('an id whose derived names collide with a listed site is refused (site would take kbf\'s prod-site environment)', () => {
    expect(() => c.siteId('docs.example.org', 'site')).toThrow(/prod-site/);
  });
  test('every name claimed by an unlisted id is disjoint from every listed site\'s', () => {
    const listed = new Set(Object.values(c.SITES).flatMap((id) => c.claimedNames(c.siteNames(id))));
    const ids = ['site', 'prod', 'docs', 'a', 'kbfx', 'tier', 'tierstore2', 'publish', 'dns', 'x1', 'router', 'headers'];
    let accepted = 0;
    for (const id of ids) {
      let ok = true;
      try { c.siteId('docs.example.org', id); } catch { ok = false; }
      if (!ok) continue; // refused ids are fine; accepted ones must be disjoint
      accepted++;
      for (const n of c.claimedNames(c.siteNames(id))) expect(listed.has(n)).toBe(false);
    }
    expect(accepted).toBeGreaterThan(5);
  });
  test('the held CLI refuses --site site on an unlisted domain', () => {
    expect(() => parse(['github', '--repo', REPO, '--reviewer', 'someone', '--domain', 'docs.example.org', '--site', 'site'])).toThrow(Refused);
  });
  test.each(['', 'Bad', '1abc', 'a-b', 'a_b', 'x'.repeat(17)])('id %j is refused', (id) => {
    expect(() => c.siteNames(id)).toThrow(/site id/);
  });
});

describe('kbf and tierstore names are disjoint', () => {
  test('no string is claimed by both', () => {
    const k = new Set(claimed(KBF));
    for (const s of claimed(TIER)) expect(k.has(s)).toBe(false);
  });
  test('every name carries its own id, so a third site is disjoint by construction', () => {
    for (const s of [TIER.zoneStack, TIER.siteStack, TIER.bucket(ACCOUNT), TIER.headersPolicy, TIER.router, TIER.role]) expect(s).toContain('tierstore');
  });
  test('the synthesized templates share no stack name, physical name or output export', () => {
    const synth = (domain: string, site?: string) => {
      const app = new App();
      const cfg = { ...base, domain, site };
      const stacks = [new ZoneStack(app, cfg, { env }), new SiteStack(app, { env, config: cfg, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' })];
      const names = new Set<string>(stacks.map((s) => s.stackName));
      for (const s of stacks) {
        const json = JSON.stringify(Template.fromStack(s).toJSON(), null, 1);
        for (const m of json.matchAll(/"(?:BucketName|RoleName|Name|FunctionName)": "([^"]+)"/g)) names.add(m[1]);
      }
      return { names, json: stacks.map((s) => JSON.stringify(Template.fromStack(s).toJSON())).join('\n') };
    };
    const k = synth('kbf.dev');
    const t = synth('tierstore.dev');
    expect(t.names.size).toBeGreaterThanOrEqual(7); // 2 stacks, bucket, headers policy, function, role, ...
    for (const n of t.names) expect(k.names.has(n)).toBe(false);
    expect(t.json).not.toMatch(/kbf/i);
    expect(t.json).toContain('tierstore.dev');
    expect(k.json).not.toContain('tierstore');
  });
  test('the tierstore role trusts its own environment and workflow, not kbf\'s', () => {
    const sub = c.oidcSubject(base, TIER);
    expect(sub).toContain(`:environment:${TIER.githubEnvironment}:`);
    expect(sub).toContain(`/${TIER.publishWorkflow}@`);
    expect(sub).not.toBe(c.oidcSubject(base));
    expect(sub).not.toContain('environment:prod-site:');
  });
});

describe('the held CLI', () => {
  const run = (argv: string[], github: string[] = []) => {
    const streams: string[] = [];
    const lines: string[] = [];
    const t: Tools = {
      capture: (cmd, args) => {
        const line = [cmd, ...args].join(' ');
        const envName = /environments\/([\w-]+)(\/deployment-branch-policies)?/.exec(line);
        if (envName) {
          const [, name, pols] = envName;
          if (!github.includes(name)) throw new Error('(HTTP 404)');
          const approval = name === TIER.approvalEnvironment;
          if (pols) return JSON.stringify({ branch_policies: approval ? [{ name: 'main', type: 'branch' }, { name: 'v*', type: 'tag' }] : [{ name: 'v*', type: 'tag' }] });
          return JSON.stringify({
            can_admins_bypass: false,
            deployment_branch_policy: { protected_branches: false, custom_branch_policies: true },
            protection_rules: approval ? [{ type: 'required_reviewers', prevent_self_review: false, reviewers: [{ type: 'User', reviewer: { id: 5 } }] }] : [],
          });
        }
        if (line.includes('oidc/customization/sub')) {
          return JSON.stringify({ use_default: false, use_immutable_subject: true, include_claim_keys: ['repo', 'context', 'job_workflow_ref'] });
        }
        if (line.includes('gh api repos/')) return JSON.stringify({ full_name: REPO, id: 222, owner: { id: 111 } });
        if (line.includes('gh api users/')) return JSON.stringify({ type: 'User', id: 5, login: 'someone' });
        if (line.includes('get-caller-identity')) return `${ACCOUNT}\n`;
        if (line.includes('list-hosted-zones-by-name')) return JSON.stringify({ HostedZones: [{ Name: `${args[args.indexOf('--dns-name') + 1]}.` }] });
        if (line.includes('describe-stacks')) {
          return JSON.stringify([
            { OutputKey: 'BucketName', OutputValue: 'b' },
            { OutputKey: 'DistributionId', OutputValue: 'd' },
            { OutputKey: 'PublishRoleArn', OutputValue: 'r' },
          ]);
        }
        return '{}';
      },
      stream: (cmd, args) => { streams.push([cmd, ...args].join(' ')); },
      log: (l) => lines.push(l),
      mkScratch: () => '/scratch/x',
      removeScratch: () => {},
      readFile: () => '',
      writeFile: () => {},
      listFiles: () => [],
    };
    main(argv, t);
    return { streams, text: lines.join('\n') };
  };
  const common = ['--account', ACCOUNT, '--profile', 'p'];

  test('--domain tierstore.dev selects the tierstore stacks; kbf stack names never appear', () => {
    const dns = run(['dns', ...common, '--domain', 'tierstore.dev']).streams.find((l) => l.includes('cdk diff'))!;
    expect(dns).toContain('cdk diff tierstore-dns-zone ');
    expect(dns).not.toContain('kbf');
    const site = run(['site', ...common, '--repo', REPO, '--domain', 'tierstore.dev']).streams.find((l) => l.includes('cdk diff'))!;
    expect(site).toContain('cdk diff tierstore-prod-site ');
    expect(site).not.toContain('kbf');
  });
  test('--site is passed to cdk only when given', () => {
    const s = run(['site', ...common, '--repo', REPO, '--domain', 'docs.example.org', '--site', 'docs']).streams.find((l) => l.includes('cdk diff'))!;
    expect(s).toContain('cdk diff docs-prod-site ');
    expect(s).toContain('-c site=docs');
    expect(run(['site', ...common, '--repo', REPO, '--domain', 'tierstore.dev']).streams.find((l) => l.includes('cdk diff'))).not.toContain('site=');
  });
  test('github prepares the tierstore environments, not prod and prod-site', () => {
    const r = run(['github', '--repo', REPO, '--reviewer', 'someone', '--domain', 'tierstore.dev']);
    expect(r.text).toContain(`environments/${TIER.approvalEnvironment} `);
    expect(r.text).toContain(`environments/${TIER.githubEnvironment} `);
    expect(r.text).not.toMatch(/environments\/prod(-site)?[ /]/);
    expect(r.text).toContain(`environment:${TIER.githubEnvironment}:job_workflow_ref:${REPO}/${TIER.publishWorkflow}@`);
  });
  test('vars reads the tierstore stack and prints TIERSTORE_SITE_* variables into the tierstore site environment', () => {
    const r = run(['vars', ...common, '--repo', REPO, '--domain', 'tierstore.dev'], [TIER.approvalEnvironment, TIER.githubEnvironment]);
    const sets = r.text.split('\n').filter((l) => l.startsWith('gh variable set'));
    expect(sets.map((l) => l.split(' ')[3])).toEqual(['TIERSTORE_SITE_BUCKET', 'TIERSTORE_SITE_DISTRIBUTION_ID', 'TIERSTORE_SITE_PUBLISH_ROLE_ARN']);
    for (const l of sets) expect(l).toContain(`--env ${TIER.githubEnvironment} `);
    expect(r.text).not.toContain('KBF_');
    expect(KBF.variables).toEqual(['KBF_SITE_BUCKET', 'KBF_SITE_DISTRIBUTION_ID', 'KBF_SITE_PUBLISH_ROLE_ARN']);
  });
  test('vars for tierstore refuses while only kbf.dev\'s environments exist', () => {
    expect(() => run(['vars', ...common, '--repo', REPO, '--domain', 'tierstore.dev'], [KBF.approvalEnvironment, KBF.githubEnvironment])).toThrow(Refused);
  });
  test('an unmapped domain without --site, and a --site that contradicts the domain, are refused at parse time', () => {
    expect(() => parse(['dns', ...common, '--domain', 'docs.example.org'])).toThrow(Refused);
    expect(() => parse(['dns', ...common, '--domain', 'tierstore.dev', '--site', 'kbf'])).toThrow(Refused);
    expect(parse(['dns', ...common, '--domain', 'tierstore.dev']).site).toBeUndefined();
  });
  test('upload and vars accept --domain', () => {
    expect(parse(['upload', ...common, '--commit', 'a'.repeat(40), '--domain', 'tierstore.dev']).domain).toBe('tierstore.dev');
    expect(parse(['vars', ...common, '--repo', REPO, '--domain', 'tierstore.dev']).domain).toBe('tierstore.dev');
  });
});
