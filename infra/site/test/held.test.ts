import { Refused, Tools, checkGithub, main, parse, shellQuote } from '../held/steps';

const ACCOUNT = '123456789012';
const REPO = 'example-org/example-repo';

interface Call { cmd: string; args: string[]; kind: 'capture' | 'stream' }

/** A fake: `answers` maps "cmd arg arg..." prefixes to stdout; anything else answers "{}". */
function fake(answers: Record<string, string | Error> = {}, failStream = '') {
  const calls: Call[] = [];
  const written: Record<string, string> = {};
  const removed: string[] = [];
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
    stream(cmd, args) {
      calls.push({ cmd, args, kind: 'stream' });
      if (failStream && [cmd, ...args].join(' ').includes(failStream)) throw new Error('failed');
    },
    log: (l) => lines.push(l),
    mkScratch: () => '/scratch/x',
    removeScratch: (d) => { removed.push(d); },
    readFile: () => '24.21.0\n',
    writeFile: (p, d) => { written[p] = d; },
    listFiles: () => ['404.html', 'assets/abc/app.js', 'assets/abc/site.css', 'index.html'],
  };
  return { t, calls, lines, written, removed, text: () => lines.join('\n'), ran: (s: string) => calls.some((c) => [c.cmd, ...c.args].join(' ').includes(s)) };
}

const GOOD_ENV = JSON.stringify({ deployment_branch_policy: { protected_branches: false, custom_branch_policies: true } });
const GOOD_POLS = JSON.stringify({ branch_policies: [{ name: 'main', type: 'branch' }] });
const GOOD_TMPL = JSON.stringify({ use_default: false, use_immutable_subject: true, include_claim_keys: ['repo', 'context', 'job_workflow_ref'] });
const STACK = JSON.stringify([
  { OutputKey: 'BucketName', OutputValue: `kbf-prod-site-${ACCOUNT}` },
  { OutputKey: 'DistributionId', OutputValue: 'EDIST123' },
  { OutputKey: 'PublishRoleArn', OutputValue: `arn:aws:iam::${ACCOUNT}:role/kbf-prod-publish` },
]);
const SHA = 'a'.repeat(40);
const answers = (over: Record<string, string | Error> = {}) => ({
  'sts get-caller-identity': ACCOUNT,
  'cat-file -t': 'commit\n',
  'node --version': 'v24.21.0\n',
  'environments/prod/deployment-branch-policies': GOOD_POLS,
  'environments/prod': GOOD_ENV,
  'oidc/customization/sub': GOOD_TMPL,
  'describe-stacks': STACK,
  'list-hosted-zones-by-name': JSON.stringify({ HostedZones: [{ Name: 'kbf.dev.' }] }),
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
    expect(f.calls.filter((c) => c.kind === 'stream')).toHaveLength(0);
    expect(f.calls.every((c) => c.cmd === 'gh' && c.args[0] === 'api' && !c.args.includes('-X'))).toBe(true);
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
  test('--no-domain: no zone lookup, passes -c noDomain=true to cdk, and the default mode does not', () => {
    const f = fake(answers({ 'list-hosted-zones-by-name': new Error('must not be called') }));
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--no-domain'], f.t);
    expect(f.ran('route53')).toBe(false);
    expect(f.ran('-c noDomain=true')).toBe(true);
    expect(f.ran('cdk diff kbf-prod-site')).toBe(true);
    expect(f.ran('cdk deploy')).toBe(false);
    const g = fake(answers());
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO], g.t);
    expect(g.ran('noDomain')).toBe(false);
    expect(g.ran('route53')).toBe(true);
  });
  test('--no-domain still refuses without the OIDC provider and on an account mismatch', () => {
    const flags = ['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--no-domain'];
    expect(() => main(flags, fake(answers({ 'get-open-id-connect-provider': new Error('NoSuchEntity') })).t)).toThrow(/OIDC provider/);
    const f = fake(answers({ 'sts get-caller-identity': '999999999999' }));
    expect(() => main(flags, f.t)).toThrow(/not the --account/);
    expect(f.calls).toHaveLength(1);
  });
  test('--no-domain is a flag of site only', () => {
    expect(() => parse(['dns', '--account', ACCOUNT, '--profile', 'p', '--no-domain'])).toThrow(/unknown flag/);
    expect(() => parse(['vars', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--no-domain'])).toThrow(/unknown flag/);
    expect(parse(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--no-domain']).noDomain).toBe(true);
  });
  test('--no-domain --apply prints the distribution domain name from the stack outputs', () => {
    const stack = JSON.stringify([{ OutputKey: 'DistributionDomainName', OutputValue: 'd111111abcdef8.cloudfront.net' }]);
    const f = fake(answers({ 'describe-stacks': stack }));
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--no-domain', '--apply'], f.t);
    expect(f.ran('cdk deploy kbf-prod-site')).toBe(true);
    expect(f.ran('-c noDomain=true')).toBe(true);
    expect(f.text()).toContain('https://d111111abcdef8.cloudfront.net/');
  });
  test('never creates an OIDC provider', () => {
    const f = fake(answers());
    main(['site', '--account', ACCOUNT, '--profile', 'p', '--repo', REPO, '--apply'], f.t);
    expect(f.ran('create-open-id-connect-provider')).toBe(false);
  });
});

describe('--repo cross-check', () => {
  const other = JSON.stringify({ full_name: 'other-owner/example-repo', id: 999, owner: { id: 888 } });
  test.each([['github', []], ['site', ['--account', ACCOUNT, '--profile', 'p']]] as [string, string[]][])(
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
  test('works for the no-domain stack: same three variables, and the cloudfront.net name is shown', () => {
    const stack = JSON.stringify([...JSON.parse(STACK), { OutputKey: 'DistributionDomainName', OutputValue: 'd111111abcdef8.cloudfront.net' }]);
    const { f, go } = run({ 'describe-stacks': stack });
    go();
    expect(f.lines.filter((l) => l.startsWith('gh variable set'))).toHaveLength(3);
    expect(f.text()).toContain('https://d111111abcdef8.cloudfront.net/');
    expect(f.ran('route53')).toBe(false);
  });
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

// temporary: removed by site-publish.yml
describe('upload', () => {
  const flags = (...extra: string[]) => ['upload', '--account', ACCOUNT, '--profile', 'p', '--commit', SHA, ...extra];
  const aws3 = (f: ReturnType<typeof fake>) => f.calls.filter((c) => c.cmd === 'aws' && c.args[0] === 's3');
  const str = (c: Call) => [c.cmd, ...c.args].join(' ');

  test('a dry run writes nothing: every s3 call is --dryrun, no invalidation, no version.txt upload', () => {
    const f = fake(answers());
    main(flags(), f.t);
    expect(aws3(f).length).toBeGreaterThanOrEqual(5);
    for (const c of aws3(f)) expect(c.args).toContain('--dryrun');
    expect(f.ran('create-invalidation')).toBe(false);
    expect(f.text()).toContain('DRY-RUN: aws cloudfront create-invalidation');
    expect(f.text()).toContain('assets/abc/app.js');
  });

  test('--apply: sync passes, then version.txt, then the invalidation last', () => {
    const f = fake(answers());
    main(flags('--apply'), f.t);
    const aws = f.calls.filter((c) => c.cmd === 'aws').map(str);
    const lastSync = aws.map((l, i) => (l.includes('s3 sync') ? i : -1)).reduce((a, b) => Math.max(a, b));
    const cp = aws.findIndex((l) => l.includes('s3 cp') && l.includes('version.txt'));
    const inv = aws.findIndex((l) => l.includes('create-invalidation'));
    expect(aws.some((l) => l.includes('--dryrun'))).toBe(false);
    expect(aws.findIndex((l) => l.includes('s3 sync'))).toBeLessThan(cp);
    expect(lastSync).toBeLessThan(cp);
    expect(cp).toBeLessThan(inv);
    expect(inv).toBe(aws.length - 1);
    expect(aws[inv]).toContain('--paths /*');
    expect(aws[inv]).toContain('--distribution-id EDIST123');
    expect(f.written['/scratch/x/version.txt']).toBe(`${SHA}\n`);
  });

  test('every sync excludes releases/ last, deletes, and targets the bucket from the stack outputs', () => {
    const f = fake(answers());
    main(flags('--apply'), f.t);
    const syncs = f.calls.filter((c) => c.cmd === 'aws' && c.args[1] === 'sync');
    expect(syncs).toHaveLength(4);
    for (const c of syncs) {
      expect(c.args).toContain('--delete');
      expect(c.args).toContain(`s3://kbf-prod-site-${ACCOUNT}/`);
      const i = c.args.lastIndexOf('--exclude');
      expect(c.args[i + 1] === 'releases/*' || c.args[i + 1] === 'version.txt').toBe(true);
      expect(c.args.join(' ')).toContain("--exclude releases/*");
      const inc = c.args.indexOf('--include');
      if (inc >= 0) expect(c.args.indexOf('--exclude', inc)).toBeGreaterThan(inc); // releases exclusion follows every include
    }
    expect(f.calls.some((c) => c.args.includes('releases/latest.json') || c.args.some((a) => a.endsWith('/releases/')))).toBe(false);
  });

  test('the final catch-all sync keeps releases/ and the live version.txt out of its --delete', () => {
    const f = fake(answers());
    main(flags('--apply'), f.t);
    const syncs = f.calls.filter((c) => c.cmd === 'aws' && c.args[1] === 'sync');
    const last = syncs[syncs.length - 1].args.join(' ');
    expect(last).not.toContain('--include');
    expect(last).toContain('--exclude releases/*');
    expect(last).toContain('--exclude version.txt');
  });

  test('content types and cache-control per class: assets immutable, pages short', () => {
    const f = fake(answers());
    main(flags('--apply'), f.t);
    const line = (inc: string) => str(f.calls.find((c) => c.args.includes(inc))!);
    expect(line('assets/*.js')).toContain('--content-type text/javascript --cache-control public, max-age=31536000, immutable');
    expect(line('assets/*.css')).toContain('--content-type text/css --cache-control public, max-age=31536000, immutable');
    expect(line('*.html')).toContain('--content-type text/html; charset=utf-8 --cache-control public, max-age=60, s-maxage=300');
  });

  test('refuses a sha that is not a commit before building, and removes nothing it did not make', () => {
    const f = fake(answers({ 'cat-file -t': new Error('bad object') }));
    expect(() => main(flags('--apply'), f.t)).toThrow(/not a commit/);
    expect(f.ran('worktree add')).toBe(false);
    const g = fake(answers({ 'cat-file -t': 'tree\n' }));
    expect(() => main(flags(), g.t)).toThrow(/not a commit/);
  });

  test('refuses when node is not the version in site/.node-version, before npm ci', () => {
    const f = fake(answers({ 'node --version': 'v20.19.4\n' }));
    expect(() => main(flags('--apply'), f.t)).toThrow(/24\.21\.0.*20\.19\.4/);
    expect(f.ran('npm ci')).toBe(false);
    expect(f.ran('s3')).toBe(false);
    expect(f.ran('worktree remove')).toBe(true); // the temporary worktree is removed even on refusal
    expect(f.removed).toEqual(['/scratch/x']);
  });

  test('refuses when check-dist fails, before any s3 call', () => {
    const f = fake(answers(), 'check-dist');
    expect(() => main(flags('--apply'), f.t)).toThrow(/check-dist failed/);
    expect(f.calls.some((c) => c.args[0] === 's3' || c.args[0] === 'cloudfront')).toBe(false);
    expect(f.ran('worktree remove')).toBe(true);
  });

  test('builds in a detached worktree at the sha, with install scripts off, and removes it afterwards', () => {
    const f = fake(answers());
    main(flags(), f.t);
    const order = f.calls.map(str);
    expect(order.findIndex((l) => l.includes(`worktree add --detach /scratch/x/wt ${SHA}`))).toBeGreaterThanOrEqual(0);
    expect(f.ran('npm ci --ignore-scripts --no-audit --no-fund')).toBe(true);
    expect(f.ran('npm run build')).toBe(true);
    expect(order.findIndex((l) => l.includes('check-dist'))).toBeLessThan(order.findIndex((l) => l.includes('s3 sync')));
    expect(order[order.length - 1]).toContain('worktree remove --force /scratch/x/wt');
  });

  test('the account guard applies, and a short or non-hex commit is refused', () => {
    const f = fake(answers({ 'sts get-caller-identity': '999999999999' }));
    expect(() => main(flags(), f.t)).toThrow(/not the --account/);
    expect(f.calls).toHaveLength(1);
    expect(() => parse(['upload', '--account', ACCOUNT, '--profile', 'p', '--commit', 'main'])).toThrow(/40-hex/);
    expect(() => parse(['upload', '--account', ACCOUNT, '--profile', 'p'])).toThrow(/--commit/);
  });
});
