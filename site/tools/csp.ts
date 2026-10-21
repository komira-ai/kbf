// The Content-Security-Policy the site is served under (infra/site/lib/site-stack.ts), parsed, and the check that
// every resource a built page loads is allowed by it. check-dist.ts refuses the markup the policy blocks outright
// (inline script and style); this is the other direction: a new kind of resource (an image, a frame, a form) that the
// policy has no directive for would be blocked in a visitor's browser, and is found here.

import { readFileSync } from 'fs';
import { join } from 'path';
import { listFiles, readTags } from './check-dist';

export type Policy = ReadonlyMap<string, readonly string[]>;

/** The policy string of infra/site/lib/site-stack.ts: the quoted directives of CONTENT_SECURITY_POLICY, joined. */
export function policyFromSource(source: string): string {
  const m = /CONTENT_SECURITY_POLICY\s*=\s*\[([\s\S]*?)\]\s*\.join\(\s*'; '\s*\)/.exec(source);
  if (m === null) throw new Error('csp: CONTENT_SECURITY_POLICY = [...].join(\'; \') not found');
  const parts = [...m[1].matchAll(/"([^"]*)"|'([^']*)'/g)].map((x) => x[1] ?? x[2]);
  return parts.join('; ');
}

export function parsePolicy(policy: string): Policy {
  const out = new Map<string, string[]>();
  for (const d of policy.split(';').map((s) => s.trim()).filter((s) => s !== '')) {
    const [name, ...sources] = d.split(/\s+/);
    out.set(name.toLowerCase(), sources);
  }
  return out;
}

/** The sources that govern a fetch directive: its own, else default-src's. */
export function sourcesFor(policy: Policy, directive: string): readonly string[] {
  return policy.get(directive) ?? policy.get('default-src') ?? [];
}

/** The fetch directive that governs each tag's resource, for tags whose attributes load something. */
function directiveOf(tag: string, attrs: [string, string][]): string | null {
  const attr = (n: string): string => attrs.find(([k]) => k === n)?.[1] ?? '';
  switch (tag) {
    case 'script':
      return 'script-src';
    case 'link': {
      const rel = attr('rel').toLowerCase().split(/\s+/);
      if (rel.includes('stylesheet')) return 'style-src';
      if (rel.includes('modulepreload')) return 'script-src';
      if (rel.includes('icon') || rel.includes('apple-touch-icon')) return 'img-src';
      return rel.includes('canonical') || rel.includes('alternate') ? null : 'default-src';
    }
    case 'img':
    case 'image':
      return 'img-src';
    case 'iframe':
    case 'frame':
      return 'frame-src';
    case 'object':
    case 'embed':
      return 'object-src';
    case 'audio':
    case 'video':
    case 'source':
    case 'track':
      return 'media-src';
    case 'form':
      return 'form-action';
    case 'base':
      return 'base-uri';
    default:
      return null;
  }
}

/** Every resource a page of dist loads that the policy does not allow from its own origin. */
export function policyViolations(policy: Policy, dir: string): string[] {
  const out: string[] = [];
  for (const f of listFiles(dir).filter((x) => x.endsWith('.html'))) {
    for (const [tag, attrs] of readTags(readFileSync(join(dir, f), 'utf8')).tags) {
      const directive = directiveOf(tag, attrs);
      if (directive === null) continue;
      // base-uri and form-action do not fall back to default-src: absent means anything, so 'none' must be named.
      const sources = directive === 'base-uri' || directive === 'form-action' ? (policy.get(directive) ?? ['*']) : sourcesFor(policy, directive);
      if (!sources.includes("'self'")) out.push(`${f}: <${tag}> is governed by ${directive} (${sources.join(' ') || 'nothing'}), which does not allow 'self'`);
    }
  }
  return out;
}
