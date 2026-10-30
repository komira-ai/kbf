// Pins every name kbf.dev's deployed stacks use today. CloudFormation REPLACES a resource whose logical id or physical name
// changes, so none of these may move when other sites are added. The golden templates under test/golden were synthesized
// from the code before names were parameterized; they must stay byte-equal.
import * as fs from 'fs';
import * as path from 'path';
import { App } from 'aws-cdk-lib';
import { Template } from 'aws-cdk-lib/assertions';
import * as c from '../lib/config';
import { ZoneStack } from '../lib/dns-stack';
import { SiteStack } from '../lib/site-stack';
import { ENVIRONMENTS, Tools, main } from '../held/steps';

const ACCOUNT = '123456789012'; // the AWS documentation placeholder
const REPO = 'example-org/example-repo';
const PROVIDER = `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;
const env = { account: ACCOUNT, region: 'us-east-1' };
const CONFIG: c.SiteConfig = { account: ACCOUNT, domain: 'kbf.dev', repo: REPO, repoId: '222', ownerId: '111' };

const pretty = (tpl: Template) => JSON.stringify(tpl.toJSON(), null, 2) + '\n';
const golden = (name: string) => fs.readFileSync(path.join(__dirname, 'golden', name), 'utf8');

describe('kbf.dev names are pinned', () => {
  test('constants', () => {
    expect(c.bucketName(ACCOUNT)).toBe('kbf-prod-site-123456789012');
    expect(c.HEADERS_POLICY_NAME).toBe('kbf-prod-site-headers');
    expect(c.ROUTER_NAME).toBe('kbf-prod-site-router');
    expect(c.ROLE_NAME).toBe('kbf-prod-publish');
    expect(c.ZONE_STACK_NAME).toBe('kbf-dns-zone');
    expect(c.SITE_STACK_NAME).toBe('kbf-prod-site');
    expect(c.GITHUB_ENVIRONMENT).toBe('prod-site');
    expect(c.APPROVAL_ENVIRONMENT).toBe('prod');
    expect(c.PUBLISH_WORKFLOW).toBe('.github/workflows/site-publish.yml');
    expect(ENVIRONMENTS.map((e) => e.name)).toEqual(['prod', 'prod-site']);
  });

  test('the stacks are named kbf-dns-zone and kbf-prod-site', () => {
    const app = new App();
    expect(new ZoneStack(app, { account: ACCOUNT, domain: 'kbf.dev' }, { env }).stackName).toBe('kbf-dns-zone');
    expect(new SiteStack(app, { env, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' }).stackName).toBe('kbf-prod-site');
  });

  test('the site template (domain mode) is byte-equal to the golden', () => {
    const stack = new SiteStack(new App(), { env, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' });
    expect(pretty(Template.fromStack(stack))).toBe(golden('kbf-site.template.json'));
  });

  test('the site template (no-domain mode) is byte-equal to the golden', () => {
    const stack = new SiteStack(new App(), { env, config: CONFIG, oidcProviderArn: PROVIDER, noDomain: true });
    expect(pretty(Template.fromStack(stack))).toBe(golden('kbf-site-nodomain.template.json'));
  });

  test('the zone template is byte-equal to the golden', () => {
    const stack = new ZoneStack(new App(), { account: ACCOUNT, domain: 'kbf.dev' }, { env });
    expect(pretty(Template.fromStack(stack))).toBe(golden('kbf-zone.template.json'));
  });

  test('the role sub names environment prod-site and the kbf publish workflow', () => {
    expect(c.oidcSubject(CONFIG)).toBe(
      `repo:example-org@111/example-repo@222:environment:prod-site:job_workflow_ref:${REPO}/.github/workflows/site-publish.yml@refs/heads/main`,
    );
  });
});

describe('the held CLI with no --domain still drives the kbf names', () => {
  const run = (argv: string[]) => {
    const streams: string[] = [];
    const t: Tools = {
      capture: (cmd, args) => {
        const line = [cmd, ...args].join(' ');
        if (line.includes('gh api repos/')) return JSON.stringify({ full_name: REPO, id: 222, owner: { id: 111 } });
        if (line.includes('get-caller-identity')) return `${ACCOUNT}\n`;
        if (line.includes('list-hosted-zones-by-name')) return JSON.stringify({ HostedZones: [{ Name: 'kbf.dev.' }] });
        return '{}';
      },
      stream: (cmd, args) => { streams.push([cmd, ...args].join(' ')); },
      log: () => {},
      mkScratch: () => '/scratch/x',
      removeScratch: () => {},
      readFile: () => '',
      writeFile: () => {},
      listFiles: () => [],
    };
    main(argv, t);
    return streams;
  };
  test('dns diffs kbf-dns-zone with -c domain=kbf.dev and no site context', () => {
    const s = run(['dns', '--account', ACCOUNT, '--profile', 'p']).find((l) => l.includes('cdk diff'))!;
    expect(s).toContain('cdk diff kbf-dns-zone ');
    expect(s).toContain('-c domain=kbf.dev');
    expect(s).not.toContain('site=');
  });
  test('site diffs kbf-prod-site with -c domain=kbf.dev and no site context', () => {
    const s = run(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO]).find((l) => l.includes('cdk diff'))!;
    expect(s).toContain('cdk diff kbf-prod-site ');
    expect(s).toContain('-c domain=kbf.dev');
    expect(s).not.toContain('site=');
  });
});
