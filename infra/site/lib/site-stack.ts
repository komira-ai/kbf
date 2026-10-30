import { Duration, LegacyStackSynthesizer, RemovalPolicy, Stack, StackProps, CfnOutput, Token } from 'aws-cdk-lib';
import * as acm from 'aws-cdk-lib/aws-certificatemanager';
import * as cloudfront from 'aws-cdk-lib/aws-cloudfront';
import * as origins from 'aws-cdk-lib/aws-cloudfront-origins';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as route53 from 'aws-cdk-lib/aws-route53';
import * as targets from 'aws-cdk-lib/aws-route53-targets';
import * as s3 from 'aws-cdk-lib/aws-s3';
import { Construct } from 'constructs';
import * as c from './config';

export interface SiteStackProps extends Omit<StackProps, 'synthesizer'> {
  config: c.SiteConfig;
  /** ARN of the account's EXISTING GitHub OIDC provider. This stack never creates one. */
  oidcProviderArn: string;
  /** Dummy-synth only (tests): skips the hosted zone lookup. Real deploys look the zone up by name. */
  hostedZoneId?: string;
  /**
   * No-domain mode: no certificate, no alias, no Route 53 lookup or records; the distribution answers on its own
   * *.cloudfront.net name with the default certificate. Every other resource and every logical id is unchanged, so a
   * later deploy without this flag adds the domain to the same bucket and distribution in place.
   */
  noDomain?: boolean;
}

/**
 * Strict CSP. The site fetches nothing, so there is no connect-src (default-src 'none' covers it). Pages must use
 * external scripts and styles from the same origin: inline <style>/<script> are blocked.
 */
export const CONTENT_SECURITY_POLICY = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
  "object-src 'none'",
].join('; ');

export const PERMISSIONS_POLICY = 'camera=(), microphone=(), geolocation=(), payment=(), usb=()';
export const NONCURRENT_VERSION_DAYS = 365;
export const ERROR_TTL = Duration.seconds(10);

/**
 * Viewer-request function (cloudfront-js-2.0): `/x/` -> `/x/index.html`. Everything under /releases/ is a download and
 * is passed through untouched (the router never rewrites it, including a path that ends in a slash).
 */
export const ROUTER_SOURCE = `function handler(event) {
  var r = event.request;
  var uri = r.uri;
  if (uri.indexOf('/releases/') === 0) return r;
  if (uri.charAt(uri.length - 1) === '/') r.uri = uri + 'index.html';
  return r;
}
`;

/**
 * Private S3 bucket + CloudFront (OAC) for the site domain, plus the role the site's publish workflow assumes. Every
 * physical name comes from the site id (config.ts siteNames); logical ids are the same for every site. The security
 * headers (CONTENT_SECURITY_POLICY, PERMISSIONS_POLICY), the router and the cache settings are one definition shared by
 * all sites: a site that needs a different policy gets its own constants here, not an edit of another site's.
 */
export class SiteStack extends Stack {
  readonly bucket: s3.Bucket;
  readonly distribution: cloudfront.Distribution;
  readonly publishRole: iam.Role;

  constructor(scope: Construct, props: SiteStackProps) {
    // No assets, so no cdk bootstrap is needed; the admin deploys with their own credentials.
    const names = c.namesOf(props.config);
    super(scope, names.siteStack, { ...props, synthesizer: new LegacyStackSynthesizer() });
    if (!Token.isUnresolved(this.region) && this.region !== c.REGION) {
      throw new Error(`must be deployed to ${c.REGION}; got ${this.region}`);
    }
    if (!/^arn:aws:iam::\d{12}:oidc-provider\/token\.actions\.githubusercontent\.com$/.test(props.oidcProviderArn)) {
      throw new Error('oidcProviderArn must be the ARN of the account GitHub OIDC provider');
    }

    this.bucket = new s3.Bucket(this, 'Bucket', {
      bucketName: names.bucket(this.account),
      blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
      encryption: s3.BucketEncryption.S3_MANAGED,
      versioned: true,
      enforceSSL: true,
      objectOwnership: s3.ObjectOwnership.BUCKET_OWNER_ENFORCED,
      removalPolicy: RemovalPolicy.RETAIN,
      lifecycleRules: [
        {
          // Only superseded versions expire (365 days, so a replaced latest.json or page stays recoverable for a year).
          // Current objects, releases included, are kept forever.
          id: 'expire-noncurrent-versions',
          noncurrentVersionExpiration: Duration.days(NONCURRENT_VERSION_DAYS),
          abortIncompleteMultipartUploadAfter: Duration.days(7),
        },
      ],
    });

    const domain = props.config.domain;
    const zone = props.noDomain
      ? undefined
      : props.hostedZoneId
        ? route53.HostedZone.fromHostedZoneAttributes(this, 'Zone', { hostedZoneId: props.hostedZoneId, zoneName: domain })
        : route53.HostedZone.fromLookup(this, 'Zone', { domainName: domain });
    const cert = zone
      ? new acm.Certificate(this, 'Cert', { domainName: domain, validation: acm.CertificateValidation.fromDns(zone) })
      : undefined;

    const headers = new cloudfront.ResponseHeadersPolicy(this, 'Headers', {
      responseHeadersPolicyName: names.headersPolicy,
      securityHeadersBehavior: {
        contentSecurityPolicy: { contentSecurityPolicy: CONTENT_SECURITY_POLICY, override: true },
        contentTypeOptions: { override: true },
        strictTransportSecurity: { accessControlMaxAge: Duration.days(730), includeSubdomains: true, override: true },
        frameOptions: { frameOption: cloudfront.HeadersFrameOption.DENY, override: true },
        referrerPolicy: { referrerPolicy: cloudfront.HeadersReferrerPolicy.NO_REFERRER, override: true },
      },
      customHeadersBehavior: {
        customHeaders: [
          { header: 'Cross-Origin-Opener-Policy', value: 'same-origin', override: true },
          { header: 'Cross-Origin-Resource-Policy', value: 'same-origin', override: true },
          { header: 'Permissions-Policy', value: PERMISSIONS_POLICY, override: true },
        ],
      },
    });

    const router = new cloudfront.Function(this, 'Router', {
      functionName: names.router,
      comment: `${names.id} site: /x/ -> /x/index.html; /releases/* untouched`,
      runtime: cloudfront.FunctionRuntime.JS_2_0,
      code: cloudfront.FunctionCode.fromInline(ROUTER_SOURCE),
    });

    // OAC has no ListBucket, so a missing key answers 403; both get the 404 page with status 404.
    const notFound = (httpStatus: number): cloudfront.ErrorResponse => ({
      httpStatus,
      responseHttpStatus: 404,
      responsePagePath: '/404.html',
      ttl: ERROR_TTL,
    });

    this.distribution = new cloudfront.Distribution(this, 'Distribution', {
      comment: `${domain} site and releases`,
      ...(cert ? { domainNames: [domain], certificate: cert } : {}),
      defaultRootObject: 'index.html',
      minimumProtocolVersion: cloudfront.SecurityPolicyProtocol.TLS_V1_2_2021,
      httpVersion: cloudfront.HttpVersion.HTTP2_AND_3,
      errorResponses: [notFound(403), notFound(404)],
      defaultBehavior: {
        origin: origins.S3BucketOrigin.withOriginAccessControl(this.bucket),
        viewerProtocolPolicy: cloudfront.ViewerProtocolPolicy.REDIRECT_TO_HTTPS,
        allowedMethods: cloudfront.AllowedMethods.ALLOW_GET_HEAD,
        cachePolicy: cloudfront.CachePolicy.CACHING_OPTIMIZED,
        responseHeadersPolicy: headers,
        functionAssociations: [{ function: router, eventType: cloudfront.FunctionEventType.VIEWER_REQUEST }],
      },
    });

    if (zone) {
      const target = route53.RecordTarget.fromAlias(new targets.CloudFrontTarget(this.distribution));
      new route53.ARecord(this, 'ARecord', { zone, recordName: domain, target });
      new route53.AaaaRecord(this, 'AaaaRecord', { zone, recordName: domain, target });
    }

    // The publish role: assumed by the kbf reusable workflow only. No CDK, no bootstrap roles, no IAM.
    this.publishRole = new iam.Role(this, 'PublishRole', {
      roleName: names.role,
      description: `${names.id} site-publish workflow: uploads to the ${domain} bucket and invalidates its distribution`,
      maxSessionDuration: Duration.hours(1),
      assumedBy: new iam.FederatedPrincipal(
        props.oidcProviderArn,
        {
          StringEquals: {
            [`${c.GITHUB_OIDC_HOST}:aud`]: c.OIDC_AUDIENCE,
            [`${c.GITHUB_OIDC_HOST}:sub`]: c.oidcSubject(props.config, names),
          },
        },
        'sts:AssumeRoleWithWebIdentity',
      ),
    });
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'ListBucket',
        actions: ['s3:ListBucket'],
        resources: [this.bucket.bucketArn],
      }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'PutObjects',
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects('*')],
      }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'DeleteOutsideReleases',
        actions: ['s3:DeleteObject'],
        resources: [this.bucket.arnForObjects('*')],
      }),
    );
    // Releases are never deleted by CI. An explicit deny holds even if a later edit widens the allow above.
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'NeverDeleteReleases',
        effect: iam.Effect.DENY,
        actions: ['s3:DeleteObject', 's3:DeleteObjectVersion'],
        resources: [this.bucket.arnForObjects(`${c.RELEASES_PREFIX}*`)],
      }),
    );
    // A release file is written once: a PutObject under releases/<version>/ must carry If-None-Match: * (S3 conditional
    // write), so a re-publish of an existing version fails instead of overwriting. releases/latest.json sits directly
    // under releases/, so the pattern releases/*/* does not match it and it may be overwritten.
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'ReleasesWriteOnce',
        effect: iam.Effect.DENY,
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects(`${c.RELEASES_PREFIX}*/*`)],
        conditions: { StringNotEquals: { 's3:if-none-match': '*' } },
      }),
    );
    this.publishRole.addToPolicy(
      new iam.PolicyStatement({
        sid: 'Invalidate',
        actions: ['cloudfront:CreateInvalidation'],
        resources: [`arn:aws:cloudfront::${this.account}:distribution/${this.distribution.distributionId}`],
      }),
    );

    new CfnOutput(this, 'BucketName', { value: this.bucket.bucketName });
    new CfnOutput(this, 'DistributionId', { value: this.distribution.distributionId });
    new CfnOutput(this, 'DistributionDomainName', { value: this.distribution.distributionDomainName });
    new CfnOutput(this, 'PublishRoleArn', { value: this.publishRole.roleArn });
  }
}
