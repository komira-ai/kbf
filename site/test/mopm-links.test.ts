// T12 (from mopm.dev's site/, with this site's docs): link rewriting in a repo doc. A relative link resolves in the
// repo as GitHub resolves it: a published doc stays on the site (also when named by file name or repo path from another
// doc), any other repo path goes to GitHub, and anything resolving outside the repo is text. A fragment on a site
// page carries the prefix of the heading ids. Schemes are matched case-insensitively. In a readme, only absolute links
// and in-page fragments survive. (links.ts is mopm's with three patches: tools/mopm/SOURCE.json lists them.)

import { describe, expect, test } from './jest-compat';
import { internalHref, safeHref, webHref } from '../tools/mopm/links';

const blob = (p: string) => `https://github.com/komira-ai/kbf/blob/main/${p}`;
const docs = (source: string) => ({ mode: 'docs', idPrefix: 'doc-', source }) as const;
const readme = { mode: 'readme', idPrefix: 'readme-' } as const;

test.each([
  ['docs/api.md', '../docs/reapi-auth.md', '/docs/reapi-auth/'],
  ['docs/api.md', 'reapi-auth.md', '/docs/reapi-auth/'],
  ['docs/api.md', './reapi-auth.md#authorizers', '/docs/reapi-auth/#doc-authorizers'],
  ['docs/api.md', 'design/fleet-updates.md', '/docs/design/fleet-updates/'],
  ['docs/api.md', 'design/fleet-updates.md#114-the-fleet', '/docs/design/fleet-updates/#doc-114-the-fleet'],
  ['docs/api.md', '/docs/design/storage.md', '/docs/design/storage/'],
  ['docs/design/daemon.md', 'storage.md', '/docs/design/storage/'],
  ['docs/design/daemon.md', '../api.md#nodes', '/docs/api/#doc-nodes'],
  ['docs/design/daemon.md', '../../ARCHITECTURE.md', '/docs/architecture/detail/'],
  ['docs/design/daemon.md', '../deploy/storage-node.md', '/docs/deploy/storage-node/'],
  ['docs/deploy/storage-node.md', '../design/daemon.md', '/docs/design/daemon/'],
  ['ARCHITECTURE.md', 'docs/design', blob('docs/design')],
  ['ARCHITECTURE.md', 'docs/design/scheduler.md', '/docs/design/scheduler/'],
  ['README.md', 'LICENSE', blob('LICENSE')],
  ['docs/design/daemon.md', '../../crates/kbf-daemon/src/main.rs#L10', blob('crates/kbf-daemon/src/main.rs#L10')],
  ['docs/design/daemon.md', '/crates/kbf-cli/tests/golden/kbf-daemon.toml', blob('crates/kbf-cli/tests/golden/kbf-daemon.toml')],
  ['docs/design/daemon.md', '../../../x', null],
  ['docs/design/daemon.md', '//github.com/komira-ai/kbf', null],
  ['docs/design/daemon.md', '#sec', '#doc-sec'],
  ['docs/design/daemon.md', 'HTTPS://github.com/komira-ai/kbf', 'https://github.com/komira-ai/kbf'],
  ['docs/design/daemon.md', 'JavaScript:alert(1)', null],
] as const)('in %s, %j -> %j', (source, raw, want) => {
  expect(safeHref(raw, docs(source))).toBe(want);
});

test.each([
  ['relative.md', null],
  ['/abs/path', null],
  ['#Install', '#readme-Install'],
  ['#caf%C3%A9', '#readme-caf%C3%A9'],
  ['#', null],
  ['HTTPS://kbf.dev/x', 'https://kbf.dev/x'],
] as const)('in a readme, %j -> %j', (raw, want) => {
  expect(safeHref(raw, readme)).toBe(want);
});

test('internalHref accepts same-site paths only', () => {
  expect(internalHref('/docs/api/')).toBe('/docs/api/');
  for (const bad of ['//evil', '/\\evil', 'https://kbf.dev', 'docs/api', '/\t/evil', '']) {
    expect(() => internalHref(bad)).toThrow();
  }
});

describe('webHref (the source link of a page)', () => {
  test.each([
    ['https://github.com/komira-ai/kbf', 'https://github.com/komira-ai/kbf'],
    ['HTTP://Example.com', 'http://example.com/'],
  ])('%s is a link to %s', (raw, want) => {
    expect(webHref(raw)).toBe(want);
  });

  test.each(['mailto:a@example.com', '#readme', 'javascript:alert(1)', 'data:text/html,x', 'ftp://example.com/', 'example.com', '//example.com'])(
    '%s is not a link',
    (raw) => {
      expect(webHref(raw)).toBeNull();
    },
  );
});
