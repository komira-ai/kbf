// The Content-Security-Policy check over the built HTML, CSS and JavaScript.
//
// Catches: markup and code the site's policy (default-src 'none'; script-src, style-src and img-src 'self'; no
// connect-src) blocks in a visitor's browser, which a build in a developer's browser with no policy would not show:
// an inline <script> or <style>, a style="" attribute, an on*= handler, a javascript: or data: URL, a script, style
// or image from another origin, an @import, a script that fetches or runs a string, a form, a frame or an <img> the
// policy has no directive for, and a policy that no longer is the one the pages were checked against.

import assert from 'node:assert/strict';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';
import { checkDist } from '../tools/check-dist';
import { parsePolicy, policyFromSource, policyViolations } from '../tools/csp';
import { assetDir, buildReal, copyTree, plant, read, REPO } from './support';

const dist = buildReal('csp');
const POLICY = policyFromSource(read(REPO, 'infra', 'site', 'lib', 'site-stack.ts'));

function refused(name: string, file: string, find: string | RegExp, replace: string, why: RegExp): void {
  test(`check-dist refuses ${name}`, () => {
    const t = copyTree(dist, `csp-${name.replace(/\W+/g, '-')}`);
    plant(t, file.replace('__ID__', assetDir(t)), find, replace);
    assert.throws(() => checkDist(t), why);
  });
}

test('the built site passes check-dist', () => {
  checkDist(dist);
});

refused('a style attribute', 'index.html', '<h1', '<h1 style="color:red"', /a style attribute on <h1>/);
refused('a style element', 'index.html', '</head>', '<style>a{}</style></head>', /a <style> element/);
refused('an inline script', 'index.html', '</head>', '<script>alert(1)</script></head>', /an inline <script>/);
refused('a script without a src under /assets/', 'index.html', 'src="/assets/', 'src="/other/', /a <script> without one src under \/assets\//);
refused('an event handler', 'index.html', '<h1', '<h1 onclick="x()"', /an event-handler attribute onclick on <h1>/);
refused('a javascript: URL', 'index.html', 'href="/install/"', 'href="javascript:alert(1)"', /a script URL/);
refused('an entity-encoded javascript: URL', 'index.html', 'href="/install/"', 'href="&#106;avascript:alert(1)"', /a script URL/);
refused('a data: URL', 'index.html', 'href="/install/"', 'href="data:text/html,x"', /a script URL/);
refused('a stylesheet from another origin', 'index.html', /href="\/assets\/[0-9a-f]+\/site\.css"/, 'href="https://cdn.example.org/x.css"', /<link href> names another origin/);
refused('a comment in a page', 'index.html', '</head>', '<!-- x --></head>', /a '<' that does not start a tag/);
refused('an @import in a stylesheet', '__ID__/site.css', /^/, '@import "other.css";\n', /an @import/);
refused('a url() to another origin in a stylesheet', '__ID__/site.css', /^/, 'a{background:url(https://cdn.example.org/x.png)}\n', /leaves the site/);
refused('a fetch in a script', '__ID__/copy.js', /^/, 'fetch("/x");\n', /uses fetch/);
refused('a string run as code', '__ID__/copy.js', /^/, 'eval("1");\n', /uses eval/);
refused('markup from a string', '__ID__/copy.js', /^/, 'document.body.innerHTML = "x";\n', /uses innerHTML/);
test('check-dist refuses a file type that is not published', () => {
  const t = copyTree(dist, 'csp-filetype');
  writeFileSync(join(t, 'robots.txt'), 'x\n');
  assert.throws(() => checkDist(t), /robots\.txt: only \.html, \.js and \.css files are published/);
});

test('the policy parsed from infra/site is the one the pages are checked against', () => {
  assert.equal(
    POLICY,
    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'",
  );
  assert.equal(POLICY.includes('connect-src'), false, 'a connect-src would let a script fetch; the pages have none');
  assert.equal(POLICY.includes('unsafe-'), false);
});

test('every resource the built pages load is allowed by that policy', () => {
  assert.deepEqual(policyViolations(parsePolicy(POLICY), dist), []);
});

test('a policy without style-src, or a page with a form, a frame or an image the policy lacks, is caught', () => {
  const noStyle = POLICY.replace("style-src 'self'; ", '');
  assert.match(policyViolations(parsePolicy(noStyle), dist).join('\n'), /<link> is governed by style-src \('none'\)/);
  const noImg = POLICY.replace("img-src 'self'; ", '');
  const t = copyTree(dist, 'csp-img');
  plant(t, 'index.html', '</main>', '<img src="/assets/x.png"></main>');
  assert.match(policyViolations(parsePolicy(noImg), t).join('\n'), /<img> is governed by img-src/);
  plant(t, 'index.html', '</main>', '<form action="/x"></form><iframe src="/x"></iframe></main>');
  const found = policyViolations(parsePolicy(POLICY), t).join('\n');
  assert.match(found, /<form> is governed by form-action/);
  assert.match(found, /<iframe> is governed by frame-src/);
});
