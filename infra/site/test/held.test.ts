import { Refused, Tools, checkGithub, main, parse } from '../held/steps';

const ACCOUNT = '123456789012';
const REPO = 'example-org/example-repo';

interface Call { cmd: string; args: string[]; kind: 'capture' | 'stream' }

/** A fake: `answers` maps "cmd arg arg..." prefixes to stdout; anything else answers "{}". */
function fake(answers: Record<string, string | Error> = {}) {
  const calls: Call[] = [];
  const lines: string[] = [];
  const t: Tools = {
    capture(cmd, args) {
      calls.push({ cmd, args, kind: 'capture' });
      const line = [cmd, ...args].join(' ');
      for (const [k, v] of Object.entries(answers)) {
        if (line.includes(k)) {
          if (v instanceof Error) throw v;
          return v;
        }
      }
      return '{}';
    },
    stream(cmd, args) { calls.push({ cmd, args, kind: 'stream' }); },
    log: (l) => lines.push(l),
  };
  return { t, calls, lines, text: () => lines.join('\n'), ran: (s: string) => calls.some((c) => [c.cmd, ...c.args].join(' ').includes(s)) };
}

const GOOD_ENV = JSON.stringify({ deployment_branch_policy: { protected_branches: false, custom_branch_policies: true } });
const GOOD_POLS = JSON.stringify({ branch_policies: [{ name: 'main', type: 'branch' }] });
const GOOD_TMPL = JSON.stringify({ use_default: false, use_immutable_subject: true, include_claim_keys: ['repo', 'context', 'job_workflow_ref'] });
const STACK = JSON.stringify([
  { OutputKey: 'BucketName', OutputValue: `kbf-prod-site-${ACCOUNT}` },
  { OutputKey: 'DistributionId', OutputValue: 'EDIST123' },
  { OutputKey: 'PublishRoleArn', OutputValue: `arn:aws:iam::${ACCOUNT}:role/kbf-prod-publish` },
]);
const answers = (over: Record<string, string | Error> = {}) => ({
  'sts get-caller-identity': ACCOUNT,
  'environments/prod/deployment-branch-policies': GOOD_POLS,
  'environments/prod': GOOD_ENV,
  'oidc/customization/sub': GOOD_TMPL,
  'describe-stacks': STACK,
  'list-hosted-zones-by-name': JSON.stringify({ HostedZones: [{ Name: 'kbf.dev.' }] }),
  [`api repos/${REPO}`]: JSON.stringify({ id: 222, owner: { id: 111 } }),
  ...over,
});

describe('flag parsing', () => {
  test('--force and unknown flags are refused', () => {
    expect(() => parse(['dns', '--account', ACCOUNT, '--profile', 'p', '--force'])).toThrow(Refused);
    expect(() => parse(['bogus'])).toThrow(Refused);
  });
  test('an account that is not 12 digits, a missing profile and a bad repo are refused', () => {
    expect(() => parse(['dns', '--account', '12', '--profile', 'p'])).toThrow(/12 digits/);
    expect(() => parse(['dns', '--account', ACCOUNT])).toThrow(/--profile/);
    expect(() => parse(['github', '--repo', 'nope'])).toThrow(/owner/);
  });
  test('vars has no --apply', () => {
    expect(() => parse(['vars', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--apply'])).toThrow(/no --apply/);
  });
  test('dry run is the default', () => {
    expect(parse(['github', '--repo', REPO]).apply).toBe(false);
    expect(parse(['github', '--repo', REPO, '--apply']).apply).toBe(true);
  });
});

describe('account guard', () => {
  test.each(['dns', 'site', 'vars'])('%s refuses before doing anything else when the profile is another account', (cmd) => {
    const f = fake(answers({ 'sts get-caller-identity': '999999999999' }));
    const flags = ['--account', ACCOUNT, '--profile', 'p', ...(cmd === 'dns' ? [] : ['--repo', REPO])];
    expect(() => main([cmd, ...flags], f.t)).toThrow(/not the --account/);
    expect(f.calls).toHaveLength(1);
  });
});

describe('dry run', () => {
  test('github prints the gh commands and executes none', () => {
    const f = fake(answers());
    main(['github', '--repo', REPO], f.t);
    expect(f.calls).toHaveLength(0);
    expect(f.text()).toContain(`DRY-RUN: gh api -X PUT repos/${REPO}/environments/prod`);
    expect(f.text()).toContain('custom_branch_policies]=true');
    expect(f.text()).toContain('name=main');
    expect(f.text()).toContain('include_claim_keys[]=job_workflow_ref');
    expect(f.text()).toContain('REMINDER');
  });
  test('github --apply runs the three writes, skipping the branch policy that exists', () => {
    const f = fake(answers());
    main(['github', '--repo', REPO, '--apply'], f.t);
    const streamed = f.calls.filter((c) => c.kind === 'stream').map((c) => c.args.join(' '));
    expect(streamed).toHaveLength(2);
    expect(streamed[0]).toContain('-X PUT');
    expect(streamed[1]).toContain('oidc/customization/sub');
  });
  test('dns and site run cdk diff, never deploy', () => {
    for (const cmd of [['dns'], ['site', '--repo', REPO]]) {
      const f = fake(answers());
      main([...cmd, '--account', ACCOUNT, '--profile', 'p'], f.t);
      expect(f.ran('cdk diff')).toBe(true);
      expect(f.ran('cdk deploy')).toBe(false);
    }
  });
  test('--apply deploys; the tests run first', () => {
    const f = fake(answers({ 'describe-stacks': JSON.stringify([{ OutputKey: 'NameServers', OutputValue: 'ns-1.example,ns-2.example,ns-3.example,ns-4.example' }]) }));
    main(['dns', '--account', ACCOUNT, '--profile', 'p', '--apply'], f.t);
    const cmds = f.calls.filter((c) => c.kind === 'stream').map((c) => c.args[0]);
    expect(cmds.indexOf('test')).toBeLessThan(cmds.indexOf('cdk'));
    expect(f.ran('cdk deploy kbf-dns-zone')).toBe(true);
    expect(f.lines.filter((l) => l.startsWith('ns-'))).toHaveLength(4);
  });
});

describe('site', () => {
  test('passes the repo and owner ids read from gh api to cdk as context', () => {
    const f = fake(answers());
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], f.t);
    expect(f.ran('-c githubRepoId=222 -c githubOwnerId=111')).toBe(true);
    expect(f.ran(`-c oidcProviderArn=arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`)).toBe(true);
  });
  test('refuses without the OIDC provider, and without the zone', () => {
    expect(() => main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], fake(answers({ 'get-open-id-connect-provider': new Error('NoSuchEntity') })).t)).toThrow(/OIDC provider/);
    expect(() => main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], fake(answers({ 'list-hosted-zones-by-name': JSON.stringify({ HostedZones: [] }) })).t)).toThrow(/hosted zone/);
  });
  test('never creates an OIDC provider', () => {
    const f = fake(answers());
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--apply'], f.t);
    expect(f.ran('create-open-id-connect-provider')).toBe(false);
  });
});

describe('vars', () => {
  const run = (over: Record<string, string | Error> = {}) => {
    const f = fake(answers(over));
    const go = () => main(['vars', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], f.t);
    return { f, go };
  };
  test('prints the three gh variable set commands when GitHub is as expected', () => {
    const { f, go } = run();
    go();
    const sets = f.lines.filter((l) => l.startsWith('gh variable set'));
    expect(sets).toHaveLength(3);
    expect(sets[0]).toBe(`gh variable set KBF_SITE_BUCKET --env prod --repo ${REPO} --body 'kbf-prod-site-${ACCOUNT}'`);
  });
  test.each([
    ['environment missing', { 'environments/prod': new Error('404') }],
    ['protected branches rather than custom', { 'environments/prod': JSON.stringify({ deployment_branch_policy: { protected_branches: true, custom_branch_policies: false } }) }],
    ['a second branch policy', { 'environments/prod/deployment-branch-policies': JSON.stringify({ branch_policies: [{ name: 'main', type: 'branch' }, { name: 'dev', type: 'branch' }] }) }],
    ['a wildcard branch policy', { 'environments/prod/deployment-branch-policies': JSON.stringify({ branch_policies: [{ name: '*', type: 'branch' }] }) }],
    ['the default sub template', { 'oidc/customization/sub': JSON.stringify({ use_default: true }) }],
    ['a sub template without job_workflow_ref', { 'oidc/customization/sub': JSON.stringify({ use_default: false, use_immutable_subject: true, include_claim_keys: ['repo', 'context'] }) }],
    ['a mutable subject', { 'oidc/customization/sub': JSON.stringify({ use_default: false, use_immutable_subject: false, include_claim_keys: ['repo', 'context', 'job_workflow_ref'] }) }],
  ] as [string, Record<string, string | Error>][])('refuses and prints nothing: %s', (_n, over) => {
    const { f, go } = run(over);
    expect(go).toThrow(Refused);
    expect(f.lines.filter((l) => l.startsWith('gh variable set'))).toHaveLength(0);
  });
  test('refuses when the stack has no outputs', () => {
    const { go } = run({ 'describe-stacks': '[]' });
    expect(go).toThrow(/no outputs/);
  });
  test('checkGithub returns no mismatch for the settings that github sets', () => {
    expect(checkGithub(fake(answers()).t, REPO)).toEqual([]);
  });
});
