import * as vm from 'vm';
import { App } from 'aws-cdk-lib';
import { Match, Template } from 'aws-cdk-lib/assertions';
import * as c from '../lib/config';
import { ZoneStack } from '../lib/dns-stack';
import { NONCURRENT_VERSION_DAYS, ROUTER_SOURCE, SiteStack } from '../lib/site-stack';

const ACCOUNT = '123456789012'; // the AWS documentation placeholder, not a real account
// Fake repository and ids: the stack takes them as context, none is built in.
const CONFIG: c.SiteConfig = { account: ACCOUNT, domain: 'kbf.dev', repo: 'example-org/example-repo', repoId: '222', ownerId: '111' };
const PROVIDER = `arn:aws:iam::${ACCOUNT}:oidc-provider/token.actions.githubusercontent.com`;
const env = { account: ACCOUNT, region: 'us-east-1' };

function site(): Template {
  const app = new App();
  return Template.fromStack(new SiteStack(app, { env, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0TESTZONE' }));
}
const t = site();

function statements(template: Template): any[] {
  const policies = template.findResources('AWS::IAM::Policy');
  const out: any[] = [];
  for (const p of Object.values(policies)) out.push(...(p as any).Properties.PolicyDocument.Statement);
  return out;
}

describe('bucket', () => {
  test('is private, encrypted, versioned, owner-enforced, retained, and named kbf-prod-site-<account>', () => {
    t.hasResourceProperties('AWS::S3::Bucket', {
      BucketName: `kbf-prod-site-${ACCOUNT}`,
      PublicAccessBlockConfiguration: {
        BlockPublicAcls: true,
        BlockPublicPolicy: true,
        IgnorePublicAcls: true,
        RestrictPublicBuckets: true,
      },
      BucketEncryption: { ServerSideEncryptionConfiguration: [{ ServerSideEncryptionByDefault: { SSEAlgorithm: 'AES256' } }] },
      VersioningConfiguration: { Status: 'Enabled' },
      OwnershipControls: { Rules: [{ ObjectOwnership: 'BucketOwnerEnforced' }] },
    });
    t.hasResource('AWS::S3::Bucket', { DeletionPolicy: 'Retain', UpdateReplacePolicy: 'Retain' });
  });

  test('only noncurrent versions expire; no rule expires current objects (releases are kept forever)', () => {
    const b = Object.values(t.findResources('AWS::S3::Bucket'))[0] as any;
    for (const r of b.Properties.LifecycleConfiguration.Rules) {
      expect(r.ExpirationInDays).toBeUndefined();
      expect(r.ExpirationDate).toBeUndefined();
      expect(r.NoncurrentVersionExpiration).toEqual({ NoncurrentDays: 365 });
    }
    expect(NONCURRENT_VERSION_DAYS).toBe(365);
  });

  test('bucket policy denies every non-TLS request on the bucket and its objects', () => {
    const pol = Object.values(t.findResources('AWS::S3::BucketPolicy'))[0] as any;
    const deny = pol.Properties.PolicyDocument.Statement.filter((s: any) => s.Effect === 'Deny');
    expect(deny).toHaveLength(1);
    expect(deny[0].Action).toBe('s3:*');
    expect(deny[0].Principal).toEqual({ AWS: '*' });
    expect(deny[0].Condition).toEqual({ Bool: { 'aws:SecureTransport': 'false' } });
    expect(deny[0].Resource).toHaveLength(2);
    expect(JSON.stringify(deny[0].Resource)).toContain('/*'); // objects as well as the bucket
  });

  test('the CloudFront allow is limited to this distribution by AWS:SourceArn', () => {
    const pol = Object.values(t.findResources('AWS::S3::BucketPolicy'))[0] as any;
    const allow = pol.Properties.PolicyDocument.Statement.filter((s: any) => s.Effect === 'Allow');
    expect(allow).toHaveLength(1);
    const cond = allow[0].Condition.StringEquals['AWS:SourceArn'];
    expect(JSON.stringify(cond)).toContain(':distribution/');
    expect(JSON.stringify(cond)).toContain('"Ref":"Distribution');
  });

  test('bucket policy denies non-TLS and grants CloudFront read only (no public principal)', () => {
    const pol = Object.values(t.findResources('AWS::S3::BucketPolicy'))[0] as any;
    for (const s of pol.Properties.PolicyDocument.Statement) {
      if (s.Effect === 'Allow') {
        expect(JSON.stringify(s.Principal)).not.toMatch(/"AWS":"\*"/);
        expect(s.Principal).toEqual({ Service: 'cloudfront.amazonaws.com' });
        expect(s.Action).toBe('s3:GetObject');
      }
    }
  });
});

describe('cloudfront', () => {
  test('uses OAC, not OAI', () => {
    t.resourceCountIs('AWS::CloudFront::OriginAccessControl', 1);
    t.resourceCountIs('AWS::CloudFront::CloudFrontOriginAccessIdentity', 0);
    t.hasResourceProperties('AWS::CloudFront::Distribution', {
      DistributionConfig: { Origins: [Match.objectLike({ S3OriginConfig: { OriginAccessIdentity: '' } })] },
    });
  });

  test('distribution: alias, root object, TLS 1.2_2021, HTTP/2+3, https redirect, GET/HEAD, 403/404 -> 404 page', () => {
    t.hasResourceProperties('AWS::CloudFront::Distribution', {
      DistributionConfig: Match.objectLike({
        Aliases: ['kbf.dev'],
        DefaultRootObject: 'index.html',
        HttpVersion: 'http2and3',
        ViewerCertificate: Match.objectLike({ MinimumProtocolVersion: 'TLSv1.2_2021', SslSupportMethod: 'sni-only' }),
        DefaultCacheBehavior: Match.objectLike({
          ViewerProtocolPolicy: 'redirect-to-https',
          AllowedMethods: ['GET', 'HEAD'],
          CachePolicyId: '658327ea-f89d-4fab-a63d-7e88639e58f6', // CACHING_OPTIMIZED
          FunctionAssociations: [Match.objectLike({ EventType: 'viewer-request' })],
        }),
        CustomErrorResponses: [
          Match.objectLike({ ErrorCode: 403, ResponseCode: 404, ResponsePagePath: '/404.html' }),
          Match.objectLike({ ErrorCode: 404, ResponseCode: 404, ResponsePagePath: '/404.html' }),
        ],
      }),
    });
  });

  test('response headers policy: strict CSP without connect-src, HSTS, nosniff, DENY, no-referrer, COOP/CORP', () => {
    t.hasResourceProperties('AWS::CloudFront::ResponseHeadersPolicy', {
      ResponseHeadersPolicyConfig: Match.objectLike({
        Name: 'kbf-prod-site-headers',
        SecurityHeadersConfig: {
          ContentSecurityPolicy: {
            Override: true,
            ContentSecurityPolicy:
              "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; base-uri 'none'; " +
              "form-action 'none'; frame-ancestors 'none'; object-src 'none'",
          },
          ContentTypeOptions: { Override: true },
          StrictTransportSecurity: { AccessControlMaxAgeSec: 63072000, IncludeSubdomains: true, Override: true },
          FrameOptions: { FrameOption: 'DENY', Override: true },
          ReferrerPolicy: { ReferrerPolicy: 'no-referrer', Override: true },
        },
        CustomHeadersConfig: {
          Items: Match.arrayWith([
            { Header: 'Cross-Origin-Opener-Policy', Value: 'same-origin', Override: true },
            { Header: 'Cross-Origin-Resource-Policy', Value: 'same-origin', Override: true },
          ]),
        },
      }),
    });
    const csp = JSON.stringify(t.toJSON());
    expect(csp).not.toContain('connect-src');
    expect(csp).not.toContain('unsafe-inline');
  });

  test('records: A and AAAA alias at the apex; cert validated by DNS', () => {
    t.hasResourceProperties('AWS::Route53::RecordSet', { Type: 'A', Name: 'kbf.dev.', AliasTarget: Match.anyValue() });
    t.hasResourceProperties('AWS::Route53::RecordSet', { Type: 'AAAA', Name: 'kbf.dev.', AliasTarget: Match.anyValue() });
    t.hasResourceProperties('AWS::CertificateManager::Certificate', { DomainName: 'kbf.dev', ValidationMethod: 'DNS' });
  });

  test('function has the agreed name and runtime', () => {
    t.hasResourceProperties('AWS::CloudFront::Function', {
      Name: 'kbf-prod-site-router',
      FunctionConfig: Match.objectLike({ Runtime: 'cloudfront-js-2.0' }),
    });
  });
});

describe('router function source', () => {
  const run = (uri: string): string => {
    const ctx: any = {};
    vm.runInNewContext(ROUTER_SOURCE, ctx);
    return ctx.handler({ request: { uri } }).uri;
  };
  test.each([
    ['/', '/index.html'],
    ['/docs/', '/docs/index.html'],
    ['/a/b/', '/a/b/index.html'],
    ['/assets/app.css', '/assets/app.css'],
    ['/docs', '/docs'],
  ])('%s -> %s', (i, o) => expect(run(i)).toBe(o));
  test.each([
    '/releases/latest.json',
    '/releases/1.2.3/kbf_1.2.3_amd64.deb',
    '/releases/1.2.3/',
    '/releases/',
  ])('%s is a download: never rewritten', (u) => expect(run(u)).toBe(u));
});

describe('publish role', () => {
  const role = () => Object.values(t.findResources('AWS::IAM::Role')).find((r: any) => r.Properties.RoleName === 'kbf-prod-publish') as any;

  test('trust: one provider, aud, and sub exactly bound to repo ids, environment, reusable workflow file and main', () => {
    const expectedSub =
      'repo:example-org@111/example-repo@222:environment:prod-site:job_workflow_ref:' +
      'example-org/example-repo/.github/workflows/site-publish.yml@refs/heads/main';
    expect(c.oidcSubject(CONFIG)).toBe(expectedSub);
    // the approved publish job runs in `prod` (contents: write): its sub must not satisfy the role
    expect(expectedSub).not.toContain('environment:prod:');
    expect(c.GITHUB_ENVIRONMENT).toBe('prod-site');
    expect(c.APPROVAL_ENVIRONMENT).toBe('prod');
    const trust = role().Properties.AssumeRolePolicyDocument.Statement;
    expect(trust).toEqual([
      {
        Effect: 'Allow',
        Action: 'sts:AssumeRoleWithWebIdentity',
        Principal: { Federated: PROVIDER },
        Condition: {
          StringEquals: {
            'token.actions.githubusercontent.com:aud': 'sts.amazonaws.com',
            'token.actions.githubusercontent.com:sub': expectedSub,
          },
        },
      },
    ]);
    expect(role().Properties.MaxSessionDuration).toBe(3600);
  });

  test('no second OIDC provider is created', () => {
    t.resourceCountIs('AWS::IAM::OIDCProvider', 0);
    t.resourceCountIs('Custom::AWSCDKOpenIdConnectProvider', 0);
  });

  test('the role has no managed policies', () => {
    expect(role().Properties.ManagedPolicyArns).toBeUndefined();
  });

  test('allowed actions are exactly s3:ListBucket, s3:PutObject, s3:DeleteObject, cloudfront:CreateInvalidation', () => {
    const allow = statements(t).filter((s) => s.Effect === 'Allow');
    const actions = allow.flatMap((s) => (Array.isArray(s.Action) ? s.Action : [s.Action])).sort();
    expect(actions).toEqual(['cloudfront:CreateInvalidation', 's3:DeleteObject', 's3:ListBucket', 's3:PutObject']);
    for (const s of allow) expect(JSON.stringify(s.Resource)).not.toContain('"*"');
  });

  test('S3 grants name this one bucket; the invalidation names this one distribution', () => {
    const bucketRef = (suffix?: string) => (suffix ? [{ 'Fn::GetAtt': [expect.any(String), 'Arn'] }, suffix] : { 'Fn::GetAtt': [expect.any(String), 'Arn'] });
    for (const s of statements(t).filter((x) => x.Effect === 'Allow' && String(x.Action).startsWith('s3:'))) {
      const r = s.Resource;
      if (s.Action === 's3:ListBucket') expect(r).toEqual(bucketRef());
      else expect(r).toEqual({ 'Fn::Join': ['', bucketRef('/*')] });
    }
    const inv = statements(t).find((s) => s.Action === 'cloudfront:CreateInvalidation');
    const flat = JSON.stringify(inv.Resource);
    expect(flat).toContain(':distribution/');
    expect(flat).toContain('Ref'); // the distribution of this stack
    expect(inv.Resource['Fn::Join'][1]).toHaveLength(2);
  });

  test('releases/* can never be deleted, whatever the allows say', () => {
    const deny = statements(t).filter((s) => s.Sid === 'NeverDeleteReleases');
    expect(deny).toHaveLength(1);
    expect(deny[0].Effect).toBe('Deny');
    expect(deny[0].Action.sort()).toEqual(['s3:DeleteObject', 's3:DeleteObjectVersion']);
    expect(JSON.stringify(deny[0].Resource)).toContain('/releases/*');
  });

  test('release files are write-once: PutObject on releases/*/* is denied unless If-None-Match is *; latest.json is not matched', () => {
    const deny = statements(t).find((x) => x.Sid === 'ReleasesWriteOnce');
    expect(deny.Effect).toBe('Deny');
    expect(deny.Action).toBe('s3:PutObject');
    expect(deny.Condition).toEqual({ StringNotEquals: { 's3:if-none-match': '*' } });
    const tail = deny.Resource['Fn::Join'][1];
    expect(tail[tail.length - 1]).toBe('/releases/*/*');
    // IAM resource globs: '*' spans '/', but the pattern needs a second '/', which releases/latest.json lacks.
    const re = new RegExp('^releases/.*/.*$');
    expect(re.test('releases/latest.json')).toBe(false);
    expect(re.test('releases/1.2.3/kbf.deb')).toBe(true);
  });

  test('the role may put under releases/ and may delete elsewhere', () => {
    const put = statements(t).find((s) => s.Action === 's3:PutObject');
    expect(put.Effect).toBe('Allow');
    const del = statements(t).find((s) => s.Action === 's3:DeleteObject' && s.Effect === 'Allow');
    expect(del).toBeDefined();
  });
});

describe('guards', () => {
  test('a wrong-region stack is refused', () => {
    expect(
      () => new SiteStack(new App(), { env: { account: ACCOUNT, region: 'eu-west-1' }, config: CONFIG, oidcProviderArn: PROVIDER, hostedZoneId: 'Z0' }),
    ).toThrow(/us-east-1/);
  });
  test('an oidcProviderArn that is not the GitHub provider is refused', () => {
    expect(
      () => new SiteStack(new App(), { env, config: CONFIG, oidcProviderArn: `arn:aws:iam::${ACCOUNT}:oidc-provider/other.example`, hostedZoneId: 'Z0' }),
    ).toThrow(/oidcProviderArn/);
  });
});

describe('zone stack', () => {
  const z = Template.fromStack(new ZoneStack(new App(), { account: ACCOUNT, domain: 'kbf.dev' }, { env }));
  test('kbf.dev public zone, retained, four name servers output', () => {
    z.hasResourceProperties('AWS::Route53::HostedZone', { Name: 'kbf.dev.' });
    z.hasResource('AWS::Route53::HostedZone', { DeletionPolicy: 'Retain', UpdateReplacePolicy: 'Retain' });
    expect(Object.keys(z.findOutputs('NameServers'))).toEqual(['NameServers']);
  });
});

describe('context parameters', () => {
  const ctx = (o: Record<string, unknown>) => (k: string) => o[k];
  const ok = { account: ACCOUNT, githubRepo: 'example-org/example-repo', githubRepoId: '222', githubOwnerId: '111' };

  test('the repo id and owner id from context decide the sub; none is built in', () => {
    const a = c.oidcSubject(c.siteConfig(ctx(ok)));
    const b = c.oidcSubject(c.siteConfig(ctx({ ...ok, githubRepoId: '333', githubOwnerId: '444' })));
    expect(a).toContain('example-org@111/example-repo@222:');
    expect(b).toContain('example-org@444/example-repo@333:');
  });
  test('the domain defaults to kbf.dev and can be overridden', () => {
    expect(c.siteConfig(ctx(ok)).domain).toBe('kbf.dev');
    expect(c.siteConfig(ctx({ ...ok, domain: 'docs.example.org', site: 'docs' })).domain).toBe('docs.example.org');
  });
  test('a domain that is not a known site needs a site id, so it never gets kbf\'s names', () => {
    expect(() => c.siteConfig(ctx({ ...ok, domain: 'docs.example.org' }))).toThrow(/site/);
  });
  test.each([
    [{ githubRepoId: undefined }, /githubRepoId/],
    [{ githubOwnerId: '0' }, /githubRepoId/],
    [{ githubRepoId: '12ab' }, /githubRepoId/],
    [{ githubRepo: 'not a repo' }, /githubRepo/],
    [{ account: '1234' }, /account/],
    [{ domain: 'Bad_Domain' }, /domain/],
  ])('bad context %j is refused', (bad, re) => {
    expect(() => c.siteConfig(ctx({ ...ok, ...bad }))).toThrow(re);
  });
});

describe('no-domain mode (before DNS delegation exists)', () => {
  const synth = (noDomain: boolean, hostedZoneId?: string) => {
    const app = new App();
    const stack = new SiteStack(app, { env, config: CONFIG, oidcProviderArn: PROVIDER, noDomain, hostedZoneId });
    return { template: Template.fromStack(stack), missing: app.synth().manifest.missing ?? [] };
  };
  const nd = synth(true); // no hostedZoneId either: nothing may need a zone
  const withDomain = synth(false, 'Z0TESTZONE');
  const ids = (tpl: Template, type: string) => Object.keys(tpl.findResources(type));

  test('creates no certificate, no Route 53 record and no alias; the distribution uses the default certificate', () => {
    nd.template.resourceCountIs('AWS::CertificateManager::Certificate', 0);
    nd.template.resourceCountIs('AWS::Route53::RecordSet', 0);
    const dist = Object.values(nd.template.findResources('AWS::CloudFront::Distribution'))[0] as any;
    const cfg = dist.Properties.DistributionConfig;
    expect(cfg.Aliases).toBeUndefined();
    expect(cfg.ViewerCertificate?.AcmCertificateArn).toBeUndefined();
    expect(JSON.stringify(nd.template.toJSON())).not.toContain('AliasTarget');
  });

  test('looks up no hosted zone: synth leaves no missing context', () => {
    expect(nd.missing).toEqual([]);
  });

  test('the domain mode still has the certificate, both records and the alias (the contrast the tests above rely on)', () => {
    withDomain.template.resourceCountIs('AWS::CertificateManager::Certificate', 1);
    withDomain.template.resourceCountIs('AWS::Route53::RecordSet', 2);
    withDomain.template.hasResourceProperties('AWS::CloudFront::Distribution', {
      DistributionConfig: Match.objectLike({ Aliases: ['kbf.dev'] }),
    });
  });

  test('bucket and distribution keep their logical ids, so adding the domain later updates them in place', () => {
    for (const type of ['AWS::S3::Bucket', 'AWS::CloudFront::Distribution']) {
      expect(ids(nd.template, type)).toHaveLength(1);
      expect(ids(nd.template, type)).toEqual(ids(withDomain.template, type));
    }
    // Every other resource of the no-domain stack exists, under the same id, in the domain stack.
    const all = (tpl: Template) => Object.keys(tpl.toJSON().Resources);
    for (const id of all(nd.template)) expect(all(withDomain.template)).toContain(id);
    // The only additions are the certificate and the two records.
    expect(all(withDomain.template).length - all(nd.template).length).toBe(3);
  });

  test('the bucket name, OAC, headers policy, router and publish role are the same in both modes', () => {
    for (const type of ['AWS::S3::Bucket', 'AWS::CloudFront::OriginAccessControl', 'AWS::CloudFront::ResponseHeadersPolicy', 'AWS::CloudFront::Function', 'AWS::IAM::Role']) {
      expect(nd.template.findResources(type)).toEqual(withDomain.template.findResources(type));
    }
  });

  test('outputs the distribution domain name; the other outputs are unchanged', () => {
    const out = nd.template.toJSON().Outputs;
    expect(Object.keys(out).sort()).toEqual(['BucketName', 'DistributionDomainName', 'DistributionId', 'PublishRoleArn']);
    expect(JSON.stringify(out.DistributionDomainName)).toContain('DomainName');
  });

  test('the router function does not look at the host, so it works on the default *.cloudfront.net name', () => {
    const ctx: any = {};
    vm.runInNewContext(ROUTER_SOURCE, ctx);
    const req = { uri: '/docs/', headers: { host: { value: 'd111111abcdef8.cloudfront.net' } } };
    const out = ctx.handler({ request: req });
    expect(out.uri).toBe('/docs/index.html');
    expect(out.headers.host.value).toBe('d111111abcdef8.cloudfront.net');
  });
});
