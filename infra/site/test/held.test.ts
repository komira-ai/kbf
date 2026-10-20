import { POLICIES, Refused, Tools, checkGithub, main, parse, shellQuote } from '../held/steps';

const ACCOUNT = '123456789012';
const REPO = 'example-org/example-repo';
const REVIEWER = 'example-approver';
const REVIEWER_ID = 4242;

interface Call { cmd: string; args: string[]; kind: 'capture' | 'stream'; input?: string }

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
    stream(cmd, args, input) { calls.push({ cmd, args, kind: 'stream', input }); },
    log: (l) => lines.push(l),
  };
  return { t, calls, lines, text: () => lines.join('\n'), ran: (s: string) => calls.some((c) => [c.cmd, ...c.args].join(' ').includes(s)) };
}

const envJson = (over: Record<string, unknown> = {}, rule: Record<string, unknown> | null = {}) =>
  JSON.stringify({
    can_admins_bypass: false,
    deployment_branch_policy: { protected_branches: false, custom_branch_policies: true },
    protection_rules: rule === null ? [] : [{ type: 'required_reviewers', prevent_self_review: false, reviewers: [{ type: 'User', reviewer: { id: REVIEWER_ID } }], ...rule }],
    ...over,
  });
const polsJson = (...p: [string, string][]) => JSON.stringify({ branch_policies: p.map(([name, type]) => ({ name, type })) });
const GOOD_ENV = envJson();
const GOOD_POLS = polsJson(['main', 'branch'], ['v*', 'tag']);
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
  [`api users/${REVIEWER}`]: JSON.stringify({ login: REVIEWER, id: REVIEWER_ID, type: 'User' }),
  [`api repos/${REPO}`]: JSON.stringify({ full_name: REPO, id: 222, owner: { id: 111 } }),
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
    expect(() => parse(['github', '--repo', 'nope', '--reviewer', REVIEWER])).toThrow(/owner/);
    expect(() => parse(['github', '--repo', REPO])).toThrow(/--reviewer is required/);
    expect(() => parse(['github', '--repo', REPO, '--reviewer', 'a b'])).toThrow(/GitHub login/);
    expect(() => parse(['oidc-provider', '--account', ACCOUNT])).toThrow(/--profile/);
    expect(() => parse(['oidc-provider', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO])).toThrow(/unknown flag/);
  });
  test('vars has no --apply', () => {
    expect(() => parse(['vars', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--apply'])).toThrow(/no --apply/);
  });
  test('dry run is the default', () => {
    expect(parse(['github', '--repo', REPO, '--reviewer', REVIEWER]).apply).toBe(false);
    expect(parse(['github', '--repo', REPO, '--reviewer', REVIEWER, '--apply']).apply).toBe(true);
    expect(parse(['oidc-provider', '--account', ACCOUNT, '--profile', 'p']).apply).toBe(false);
  });
});

describe('account guard', () => {
  test.each(['dns', 'site', 'vars', 'oidc-provider'])('%s refuses before doing anything else when the profile is another account', (cmd) => {
    const f = fake(answers({ 'sts get-caller-identity': '999999999999' }));
    const flags = ['--account', ACCOUNT, '--profile', 'p', ...(cmd === 'dns' || cmd === 'oidc-provider' ? [] : ['--repo', REPO])];
    expect(() => main([cmd, ...flags], f.t)).toThrow(/not the --account/);
    expect(f.calls).toHaveLength(1);
  });
});

describe('dry run', () => {
  const GH = ['github', '--repo', REPO, '--reviewer', REVIEWER];
  const NOTHING = { 'environments/prod/deployment-branch-policies': new Error('404'), 'environments/prod': new Error('404') };
  test('github prints the gh commands and executes none', () => {
    const f = fake(answers(NOTHING));
    main(GH, f.t);
    expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
    expect(f.calls.every((c) => c.cmd === 'gh' && c.args[0] === 'api' && !c.args.includes('-X'))).toBe(true);
    expect(f.text()).toContain(`DRY-RUN: gh api -X PUT repos/${REPO}/environments/prod --input -`);
    expect(f.text()).toContain(`"reviewers":[{"type":"User","id":${REVIEWER_ID}}]`);
    expect(f.text()).toContain('"prevent_self_review":false');
    expect(f.text()).toContain('"can_admins_bypass":false');
    expect(f.text()).toContain('"custom_branch_policies":true');
    expect(f.text()).toContain('-f name=main -f type=branch');
    expect(f.text()).toContain('-f name=v* -f type=tag');
    expect(f.text()).toContain('include_claim_keys[]=job_workflow_ref');
    expect(f.text()).toContain('REMINDER');
  });
  test('github --apply on a bare repo runs the environment write with the body on stdin, both policies, and the sub template', () => {
    const f = fake(answers(NOTHING));
    main([...GH, '--apply'], f.t);
    const streamed = f.calls.filter((c) => c.kind === 'stream');
    expect(streamed.map((c) => c.args.slice(0, 3).join(' '))).toEqual([
      `api -X PUT`, `api -X POST`, `api -X POST`, `api -X PUT`,
    ].map((x) => x));
    expect(streamed[0].args).toContain(`repos/${REPO}/environments/prod`);
    expect(JSON.parse(streamed[0].input!)).toEqual({
      reviewers: [{ type: 'User', id: REVIEWER_ID }],
      prevent_self_review: false,
      can_admins_bypass: false,
      deployment_branch_policy: { protected_branches: false, custom_branch_policies: true },
    });
    expect(streamed[1].args.join(' ')).toContain('-f name=main -f type=branch');
    expect(streamed[2].args.join(' ')).toContain('-f name=v* -f type=tag');
    expect(streamed[3].args.join(' ')).toContain('oidc/customization/sub');
  });
  test('github --apply is idempotent: with everything present only the (idempotent) sub template is written', () => {
    const f = fake(answers());
    main([...GH, '--apply'], f.t);
    const streamed = f.calls.filter((c) => c.kind === 'stream').map((c) => c.args.join(' '));
    expect(streamed).toHaveLength(1);
    expect(streamed[0]).toContain('oidc/customization/sub');
    expect(f.text()).toContain('policy main/branch already present');
    expect(f.text()).toContain('policy v*/tag already present');
    expect(f.text()).toContain('already has this reviewer');
  });
  test('github adds only the missing tag policy, and only the missing main policy', () => {
    for (const [have, missing] of [[['main', 'branch'], 'name=v* -f type=tag'], [['v*', 'tag'], 'name=main -f type=branch']] as [[string, string], string][]) {
      const f = fake(answers({ 'environments/prod/deployment-branch-policies': polsJson(have) }));
      main([...GH, '--apply'], f.t);
      const posts = f.calls.filter((c) => c.kind === 'stream' && c.args.includes('POST'));
      expect(posts).toHaveLength(1);
      expect(posts[0].args.join(' ')).toContain(missing);
    }
  });
  test.each([
    ['another reviewer', envJson({}, { reviewers: [{ type: 'User', reviewer: { id: 7 } }] })],
    ['two reviewers', envJson({}, { reviewers: [{ type: 'User', reviewer: { id: REVIEWER_ID } }, { type: 'User', reviewer: { id: 7 } }] })],
    ['no reviewer rule', envJson({}, null)],
    ['self-review prevented', envJson({}, { prevent_self_review: true })],
    ['admins can bypass', envJson({ can_admins_bypass: true })],
    ['protected branches rather than custom', envJson({ deployment_branch_policy: { protected_branches: true, custom_branch_policies: false } })],
  ] as [string, string][])('github rewrites the environment when %s', (_n, env) => {
    const f = fake(answers({ 'environments/prod': env }));
    main([...GH, '--apply'], f.t);
    const puts = f.calls.filter((c) => c.kind === 'stream' && c.args.includes('repos/' + REPO + '/environments/prod'));
    expect(puts).toHaveLength(1);
    expect(JSON.parse(puts[0].input!).reviewers).toEqual([{ type: 'User', id: REVIEWER_ID }]);
  });
  test('github warns about an extra policy and does not delete it', () => {
    const f = fake(answers({ 'environments/prod/deployment-branch-policies': polsJson(['main', 'branch'], ['v*', 'tag'], ['dev', 'branch']) }));
    main([...GH, '--apply'], f.t);
    expect(f.text()).toContain('WARNING: extra policy dev/branch');
    expect(f.calls.some((c) => c.args.includes('DELETE'))).toBe(false);
  });
  test('github refuses a reviewer that is not a user, before any write', () => {
    const f = fake(answers({ [`api users/${REVIEWER}`]: JSON.stringify({ login: REVIEWER, id: 5, type: 'Organization' }) }));
    expect(() => main([...GH, '--apply'], f.t)).toThrow(/not a GitHub user/);
    expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
  });
  test('the reviewer is never baked into the source', () => {
    const src = require('fs').readFileSync(require('path').join(__dirname, '..', 'held', 'steps.ts'), 'utf8');
    expect(src).not.toMatch(/reviewers: \[\{ type: 'User', id: \d/);
    expect(src).not.toContain(REVIEWER);
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

describe('--repo cross-check', () => {
  const other = JSON.stringify({ full_name: 'other-owner/example-repo', id: 999, owner: { id: 888 } });
  test.each([['github', ['--reviewer', REVIEWER]], ['site', ['--account', ACCOUNT, '--profile', 'p']]] as [string, string[]][])(
    '%s refuses a --repo whose GitHub full_name differs, before any write or cdk call',
    (cmd, extra) => {
      const f = fake(answers({ [`api repos/${REPO}`]: other }));
      expect(() => main([cmd, '--repo', REPO, ...extra, '--apply'], f.t)).toThrow(/resolves to 'other-owner\/example-repo'/);
      expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
    },
  );
  test('the comparison ignores case, and the three values are printed before cdk runs', () => {
    const f = fake(answers({ [`api repos/${REPO.toUpperCase()}`]: JSON.stringify({ full_name: REPO, id: 222, owner: { id: 111 } }) }));
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO.toUpperCase()], f.t);
    expect(f.ran(`-c githubRepo=${REPO} `)).toBe(true); // GitHub's spelling reaches the trust policy
    const shown = f.lines.findIndex((l) => l.includes('repo id 222, owner id 111'));
    expect(f.lines[shown]).toContain(REPO);
    expect(shown).toBeGreaterThanOrEqual(0);
    expect(f.ran('cdk diff')).toBe(true);
  });
});

describe('quoting', () => {
  test('shellQuote escapes single quotes', () => {
    expect(shellQuote("a'b")).toBe(`'a'\\''b'`);
    expect(shellQuote('plain')).toBe("'plain'");
  });
  test('vars prints a value with a single quote as one safe word', () => {
    const f = fake(answers({ 'describe-stacks': JSON.stringify([
      { OutputKey: 'BucketName', OutputValue: "b'; echo pwned; '" },
      { OutputKey: 'DistributionId', OutputValue: 'E1' },
      { OutputKey: 'PublishRoleArn', OutputValue: 'arn:x' },
    ]) }));
    main(['vars', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], f.t);
    expect(f.lines.find((l) => l.includes('KBF_SITE_BUCKET'))).toContain(`--body 'b'\\''; echo pwned; '\\'''`);
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
    ['a third policy', { 'environments/prod/deployment-branch-policies': polsJson(['main', 'branch'], ['v*', 'tag'], ['dev', 'branch']) }],
    ['only the main branch policy (the tag publish job would be rejected)', { 'environments/prod/deployment-branch-policies': polsJson(['main', 'branch']) }],
    ['only the v* tag policy', { 'environments/prod/deployment-branch-policies': polsJson(['v*', 'tag']) }],
    ['v* as a branch policy, not a tag policy', { 'environments/prod/deployment-branch-policies': polsJson(['main', 'branch'], ['v*', 'branch']) }],
    ['a wildcard branch policy', { 'environments/prod/deployment-branch-policies': polsJson(['*', 'branch'], ['v*', 'tag']) }],
    ['no required reviewer', { 'environments/prod': envJson({}, null) }],
    ['an empty reviewer list', { 'environments/prod': envJson({}, { reviewers: [] }) }],
    ['self-review prevented', { 'environments/prod': envJson({}, { prevent_self_review: true }) }],
    ['admin bypass allowed', { 'environments/prod': envJson({ can_admins_bypass: true }) }],
    ['admin bypass unreported', { 'environments/prod': envJson({ can_admins_bypass: undefined }) }],
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
  test('checkGithub returns no mismatch for the settings that github sets, in either policy order', () => {
    expect(checkGithub(fake(answers()).t, REPO)).toEqual([]);
    const rev = fake(answers({ 'environments/prod/deployment-branch-policies': polsJson(['v*', 'tag'], ['main', 'branch']) }));
    expect(checkGithub(rev.t, REPO)).toEqual([]);
  });
  test('the policy set is exactly main (branch) and v* (tag)', () => {
    expect(POLICIES).toEqual([['main', 'branch'], ['v*', 'tag']]);
  });
});

describe('oidc-provider', () => {
  const ARGS = ['oidc-provider', '--account', ACCOUNT, '--profile', 'p'];
  const ABSENT = { 'list-open-id-connect-providers': JSON.stringify({ OpenIDConnectProviderList: [{ Arn: `arn:aws:iam::${ACCOUNT}:oidc-provider/other.example.com` }] }) };
  const PRESENT = { 'list-open-id-connect-providers': JSON.stringify({ OpenIDConnectProviderList: [{ Arn: `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com` }] }) };
  const CREATE = 'iam create-open-id-connect-provider --url https://token.actions.githubusercontent.com --client-id-list sts.amazonaws.com --profile p';
  test('dry run when absent prints the exact create and runs nothing', () => {
    const f = fake(answers(ABSENT));
    main(ARGS, f.t);
    expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
    expect(f.text()).toContain(`DRY-RUN: aws ${CREATE}`);
  });
  test('--apply when absent creates exactly once', () => {
    const f = fake(answers(ABSENT));
    main([...ARGS, '--apply'], f.t);
    const streamed = f.calls.filter((c) => c.kind === 'stream');
    expect(streamed).toHaveLength(1);
    expect(streamed[0].args.join(' ')).toBe(CREATE);
  });
  test.each([[false], [true]])('when present (apply=%s) nothing is created', (apply) => {
    const f = fake(answers(PRESENT));
    main([...ARGS, ...(apply ? ['--apply'] : [])], f.t);
    expect(f.ran('create-open-id-connect-provider')).toBe(false);
    expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
    expect(f.text()).toContain('already present');
  });
  test('an empty provider list counts as absent', () => {
    const f = fake(answers({ 'list-open-id-connect-providers': '{}' }));
    main([...ARGS, '--apply'], f.t);
    expect(f.ran(CREATE)).toBe(true);
  });
  test('the site step still never creates one and now points at this step', () => {
    expect(() => main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], fake(answers({ 'get-open-id-connect-provider': new Error('NoSuchEntity') })).t)).toThrow(/held -- oidc-provider --apply/);
  });
});
