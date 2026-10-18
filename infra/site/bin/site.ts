import { App } from 'aws-cdk-lib';
import { REGION, siteConfig, zoneConfig } from '../lib/config';
import { ZoneStack } from '../lib/dns-stack';
import { SiteStack } from '../lib/site-stack';

// Stacks are selected by name on the cdk command line (held/main.ts). Everything specific to an installation is
// context: -c account=<12 digits>, -c domain=<name> (default kbf.dev), and for the site stack -c githubRepo=<owner>/<name>
// -c githubRepoId=<id> -c githubOwnerId=<id> (and optionally -c oidcProviderArn=<arn>).
const app = new App();
const get = (k: string) => app.node.tryGetContext(k);
const zone = zoneConfig(get);
const env = { account: zone.account, region: REGION };

new ZoneStack(app, zone, { env });
if (get('githubRepoId') !== undefined) {
  new SiteStack(app, {
    env,
    config: siteConfig(get),
    oidcProviderArn:
      (get('oidcProviderArn') as string | undefined) ??
      `arn:aws:iam::${zone.account}:oidc-provider/token.actions.githubusercontent.com`,
  });
}
