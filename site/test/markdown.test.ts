// The Markdown subset renders as CommonMark/GFM would, and what is not supported stays literal text.
import { beforeEach, describe, expect, test } from './jest-compat';
import { MAX_DEPTH, parse } from '../tools/mopm/markdown/parse';
import { Slugger, slugify } from '../tools/mopm/markdown/render';
import { readTags } from '../tools/check-dist';
import { md, MD_TAGS, mdFull } from './md';

describe('blocks', () => {
  test('ATX headings, with closing hashes and a shift', () => {
    expect(md('# Title\n## Sub ##\n###### six')).toBe(
      '<h1 id="readme-title">Title</h1><h2 id="readme-sub">Sub</h2><h6 id="readme-six">six</h6>',
    );
    expect(md('# a\n###### b', { idPrefix: '', headingShift: 1, links: { mode: 'readme' } })).toBe(
      '<h2 id="a">a</h2><h6 id="b">b</h6>',
    );
    expect(md('#nospace')).toBe('<p>#nospace</p>');
  });

  test('paragraphs join lines; two trailing spaces or a backslash make a hard break', () => {
    expect(md('a\nb\n\nc')).toBe('<p>a\nb</p><p>c</p>');
    expect(md('a  \nb\\\nc')).toBe('<p>a<br>b<br>c</p>');
  });

  test('fenced code keeps its text verbatim and escapes it', () => {
    expect(md('```mojo\nfn f() -> Int:\n    return 1 < 2\n```')).toBe(
      '<pre tabindex="0"><code class="language-mojo">fn f() -&gt; Int:\n    return 1 &lt; 2</code></pre>',
    );
    expect(md('~~~\n```\n~~~')).toBe('<pre tabindex="0"><code>```</code></pre>');
    expect(md('```bad"lang\nx\n```')).toBe('<pre tabindex="0"><code>x</code></pre>');
    expect(md('```\nunclosed')).toBe('<pre tabindex="0"><code>unclosed</code></pre>');
  });

  test('indented code', () => {
    expect(md('    a\n\n    b\n')).toBe('<pre tabindex="0"><code>a\n\nb</code></pre>');
    expect(md('para\n    not code')).toBe('<p>para\nnot code</p>');
  });

  test('thematic breaks', () => {
    expect(md('***\n- - -\n___')).toBe('<hr><hr><hr>');
  });

  test('block quotes, with lazy continuation', () => {
    expect(md('> a\nb\n> # h')).toBe('<blockquote><p>a\nb</p><h1 id="readme-h">h</h1></blockquote>');
  });

  test('tight and loose lists, nested lists, ordered start', () => {
    expect(md('- a\n- b')).toBe('<ul><li>a</li><li>b</li></ul>');
    expect(md('- a\n\n- b')).toBe('<ul><li><p>a</p></li><li><p>b</p></li></ul>');
    expect(md('1. one\n   more\n2. two\n   - x\n   - y')).toBe(
      '<ol><li>one\nmore</li><li>two<ul><li>x</li><li>y</li></ul></li></ol>',
    );
    expect(md('3) c\n4) d')).toBe('<ol start="3"><li>c</li><li>d</li></ol>');
    expect(md('- a\n+ b')).toBe('<ul><li>a</li></ul><ul><li>b</li></ul>');
    expect(md('* lazy\ncontinued')).toBe('<ul><li>lazy\ncontinued</li></ul>');
    expect(md('- a\n\n  second para')).toBe('<ul><li><p>a</p><p>second para</p></li></ul>');
  });

  test('an ordered list interrupts a paragraph only when it starts at 1', () => {
    expect(md('text\n2. no')).toBe('<p>text\n2. no</p>');
    expect(md('text\n1. yes')).toBe('<p>text</p><ol><li>yes</li></ol>');
  });

  test('GFM tables with alignment, escaped pipes and ragged rows', () => {
    expect(md('| a | b | c |\n|:--|--:|:-:|\n| 1 | `x\\|y` |\n| 4 | 5 | 6 | 7 |')).toBe(
      '<div class="scroll" role="region" tabindex="0" aria-label="Table: a, b, c"><table><thead><tr>' +
        '<th scope="col" class="align-left">a</th><th scope="col" class="align-right">b</th>' +
        '<th scope="col" class="align-center">c</th></tr></thead><tbody>' +
        '<tr><td class="align-left">1</td><td class="align-right"><code>x|y</code></td></tr>' +
        '<tr><td class="align-left">4</td><td class="align-right">5</td><td class="align-center">6</td></tr></tbody></table></div>',
    );
    expect(md('a | b\n--- | ---')).toBe(
      '<div class="scroll" role="region" tabindex="0" aria-label="Table: a, b"><table><thead><tr><th scope="col">a</th>' +
        '<th scope="col">b</th></tr></thead></table></div>',
    );
    expect(md('a | b\n--- | --- | ---')).toBe('<p>a | b\n--- | --- | ---</p>');
  });
});

describe('wide blocks scroll on their own, reachable from the keyboard', () => {
  // At 320px a long code line or a wide table must not scroll the whole page. A table sits in a focusable, named
  // scroll region (CSS on the table itself would strip its table semantics); a code block is focusable itself.
  test('a table is wrapped in a named, focusable scroll region', () => {
    expect(md('a | *b*\n--- | ---\n1 | 2')).toBe(
      '<div class="scroll" role="region" tabindex="0" aria-label="Table: a, b"><table><thead><tr><th scope="col">a</th>' +
        '<th scope="col"><em>b</em></th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table></div>',
    );
  });

  test('a table with empty header cells still gets a label', () => {
    expect(md('| |\n|---|\n| x |')).toMatch(/^<div class="scroll" role="region" tabindex="0" aria-label="Table">/);
  });

  test('a code block is focusable', () => {
    expect(md('```\nx\n```')).toBe('<pre tabindex="0"><code>x</code></pre>');
    expect(md('    y')).toBe('<pre tabindex="0"><code>y</code></pre>');
  });
});

describe('inlines', () => {
  test('emphasis and strong emphasis follow the flanking rules', () => {
    expect(md('*a* **b** _c_ __d__ ***e***')).toBe(
      '<p><em>a</em> <strong>b</strong> <em>c</em> <strong>d</strong> <em><strong>e</strong></em></p>',
    );
    expect(md('snake_case_name and 2*3*4')).toBe('<p>snake_case_name and 2<em>3</em>4</p>');
    expect(md('* not emphasis *')).toBe('<ul><li>not emphasis *</li></ul>');
    expect(md('a * b * c')).toBe('<p>a * b * c</p>');
    expect(md('**unclosed')).toBe('<p>**unclosed</p>');
    expect(md('*a **b** c*')).toBe('<p><em>a <strong>b</strong> c</em></p>');
  });

  test('code spans take precedence and keep their text', () => {
    expect(md('`a*b*c` and ``x ` y``')).toBe('<p><code>a*b*c</code> and <code>x ` y</code></p>');
    expect(md('`unclosed *em*')).toBe('<p>`unclosed <em>em</em></p>');
    expect(md('` <b> `')).toBe('<p><code>&lt;b&gt;</code></p>');
  });

  test('backslash escapes', () => {
    expect(md('\\*not\\* \\[x\\](y)')).toBe('<p>*not* [x](y)</p>');
  });

  test('inline links, titles, angle destinations and balanced parentheses', () => {
    const rel = 'rel="nofollow noopener noreferrer ugc"';
    expect(md('[a](https://mopm.dev/p "t")')).toBe(`<p><a href="https://mopm.dev/p" ${rel}>a</a></p>`);
    expect(md('[a](<https://mopm.dev/a b>)')).toBe(`<p><a href="https://mopm.dev/ab" ${rel}>a</a></p>`);
    expect(md('[w](https://mopm.dev/wiki/A_(b))')).toBe(
      `<p><a href="https://mopm.dev/wiki/A_(b)" ${rel}>w</a></p>`,
    );
    expect(md('[*em* `c`](https://mopm.dev)')).toBe(`<p><a href="https://mopm.dev/" ${rel}><em>em</em> <code>c</code></a></p>`);
    expect(md('[no](https://mopm.dev')).toBe('<p>[no](https://mopm.dev</p>');
    expect(md('[a] [b][c]')).toBe('<p>[a] [b][c]</p>');
  });

  test('no links inside links', () => {
    expect(md('[a [b](https://mopm.dev/b) c](https://mopm.dev/a)')).toBe(
      '<p>[a <a href="https://mopm.dev/b" rel="nofollow noopener noreferrer ugc">b</a> c](https://mopm.dev/a)</p>',
    );
  });

  test('autolinks', () => {
    expect(md('<https://mopm.dev> <me@mopm.dev>')).toBe(
      '<p><a href="https://mopm.dev/" rel="nofollow noopener noreferrer ugc">https://mopm.dev</a> ' +
        '<a href="mailto:me@mopm.dev" rel="nofollow noopener noreferrer ugc">me@mopm.dev</a></p>',
    );
  });

  test('images are never loaded: they become a link reading "image: alt"', () => {
    expect(md('![logo *x*](https://mopm.dev/a.png)')).toBe(
      '<p><a href="https://mopm.dev/a.png" rel="nofollow noopener noreferrer ugc">image: logo x</a></p>',
    );
    expect(md('![](https://mopm.dev/a.png)')).toContain('>image</a>');
    expect(md('![alt](rel.png)')).toBe('<p>image: alt</p>');
  });

  test('an image inside a link (a readme badge) is the link text, never a second anchor', () => {
    const rel = 'rel="nofollow noopener noreferrer ugc"';
    expect(md('[![CI](https://mopm.dev/ci.svg)](https://mopm.dev/actions)')).toBe(
      `<p><a href="https://mopm.dev/actions" ${rel}>CI</a></p>`,
    );
    expect(md('[![CI](https://mopm.dev/a.svg)](https://mopm.dev/a) [![License](https://mopm.dev/l.svg)](https://mopm.dev/l)')).toBe(
      `<p><a href="https://mopm.dev/a" ${rel}>CI</a> <a href="https://mopm.dev/l" ${rel}>License</a></p>`,
    );
    expect(md('[see *![logo](rel.png)* here](https://mopm.dev)')).toBe(`<p><a href="https://mopm.dev/" ${rel}>see <em>logo</em> here</a></p>`);
    // A refused outer link leaves its content outside any link, so the image is a link of its own again.
    expect(md('[![CI](https://mopm.dev/ci.svg)](javascript:x)')).toBe(
      `<p><a href="https://mopm.dev/ci.svg" ${rel}>image: CI</a></p>`,
    );
  });

  test('an autolink inside a link is the link text, never a second anchor', () => {
    const rel = 'rel="nofollow noopener noreferrer ugc"';
    expect(md('[<https://mopm.dev/b>](https://mopm.dev/a)')).toBe(
      `<p><a href="https://mopm.dev/a" ${rel}>https://mopm.dev/b</a></p>`,
    );
    expect(md('[a <https://mopm.dev/b> c](https://mopm.dev/y)')).toBe(
      `<p><a href="https://mopm.dev/y" ${rel}>a https://mopm.dev/b c</a></p>`,
    );
    expect(md('[x <a@mopm.dev>](https://mopm.dev/a)')).toBe(`<p><a href="https://mopm.dev/a" ${rel}>x a@mopm.dev</a></p>`);
    expect(md('[*<https://mopm.dev/b>*](https://mopm.dev/a)')).toBe(
      `<p><a href="https://mopm.dev/a" ${rel}><em>https://mopm.dev/b</em></a></p>`,
    );
    // Outside a link the same autolinks are anchors.
    expect(md('<https://mopm.dev/b> <a@mopm.dev>')).toBe(
      `<p><a href="https://mopm.dev/b" ${rel}>https://mopm.dev/b</a> <a href="mailto:a@mopm.dev" ${rel}>a@mopm.dev</a></p>`,
    );
  });

  test('a link with no text shows its target, so it always has an accessible name', () => {
    const rel = 'rel="nofollow noopener noreferrer ugc"';
    expect(md('[](https://mopm.dev)')).toBe(`<p><a href="https://mopm.dev/" ${rel}>https://mopm.dev/</a></p>`);
    expect(md('[ ](https://mopm.dev/x)')).toBe(`<p><a href="https://mopm.dev/x" ${rel}>https://mopm.dev/x</a></p>`);
    expect(md('[![](https://mopm.dev/b.svg)](https://mopm.dev)')).toBe(`<p><a href="https://mopm.dev/" ${rel}>https://mopm.dev/</a></p>`);
    expect(md('[`x`](https://mopm.dev)')).toBe(`<p><a href="https://mopm.dev/" ${rel}><code>x</code></a></p>`);
  });

  test('in-page fragments get the id prefix', () => {
    expect(md('# Install\n[see](#install)')).toBe('<h1 id="readme-install">Install</h1><p><a href="#readme-install">see</a></p>');
  });
});

describe('literal fallbacks', () => {
  test('raw HTML is text, counted once per tag', () => {
    const r = mdFull('<div onclick="x()">hi</div>\n\n<!-- c -->');
    expect(r.html).toBe('<p>&lt;div onclick="x()"&gt;hi&lt;/div&gt;</p><p>&lt;!-- c --&gt;</p>');
    expect(r.fallbacks).toBe(3);
  });

  test('setext underlines and reference definitions stay text, counted', () => {
    const r = mdFull('Title\n=====\n\n[x]: https://mopm.dev');
    expect(r.html).toBe('<p>Title</p><p>=====</p><p>[x]: https://mopm.dev</p>');
    expect(r.fallbacks).toBe(2);
  });

  test('entities are not decoded', () => {
    expect(md('&amp; &#60;')).toBe('<p>&amp;amp; &amp;#60;</p>');
  });

  test(`containers nest at most ${MAX_DEPTH} deep; deeper content is literal`, () => {
    const r = mdFull('>'.repeat(40) + ' deep');
    expect(r.html.split('<blockquote>').length - 1).toBe(MAX_DEPTH);
    expect(r.html).toContain('<p>' + '&gt;'.repeat(40 - MAX_DEPTH) + ' deep</p>');
    expect(r.fallbacks).toBe(1);
  });

  test('deeply nested emphasis keeps its text past the inline depth cap', () => {
    const n = 5000;
    const src = '*a '.repeat(n) + 'x' + ' a*'.repeat(n);
    const { blocks } = parse(src);
    expect(blocks).toHaveLength(1);
    const html = md(src);
    expect(html.split('<em>').length - 1).toBeLessThanOrEqual(40);
    expect(html.replace(/<[^>]+>/g, '').replace(/\s+/g, ' ')).toBe(`${'a '.repeat(n)}x${' a'.repeat(n)}`);
  });
});

describe('slugs', () => {
  test('GitHub-style slugs', () => {
    expect(slugify('`[package]` display fields')).toBe('package-display-fields');
    expect(slugify('Retries and FLAKY')).toBe('retries-and-flaky');
    expect(slugify('Ünïcode & co_de')).toBe('ünïcode--co_de');
  });

  test('repeats are numbered and never collide', () => {
    const s = new Slugger();
    expect(['A', 'A', 'A-1', 'A', '!!!'].map((t) => s.slug(t))).toEqual(['a', 'a-1', 'a-1-1', 'a-2', 'section']);
  });

  test('rendered headings get unique ids', () => {
    expect(md('# A\n# A\n# A-1')).toBe('<h1 id="readme-a">A</h1><h1 id="readme-a-1">A</h1><h1 id="readme-a-1-1">A-1</h1>');
  });
});

// The docs option (repo docs at build time): heading anchors, code blocks in copy boxes, and the h2 and h3 headings
// reported for an on-this-page list. A readme gets none of it (every test above renders without the option).
describe('docs pages', () => {
  const heard: [number, string, string][] = [];
  const DOCS = { idPrefix: 'doc-', headingShift: 0, links: { mode: 'docs' as const, source: 'docs/x.md' }, docs: { onHeading: (l: number, id: string, t: string) => void heard.push([l, id, t]) } };
  beforeEach(() => heard.splice(0));

  // Mutant: the anchor linking a different id, or wrapping a heading that holds a link (a link inside a link).
  test('a heading is a link to its own anchor, unless it holds a link', () => {
    expect(md('## Retries and `FLAKY`', DOCS)).toBe('<h2 id="doc-retries-and-flaky"><a href="#doc-retries-and-flaky" class="anchor">Retries and <code>FLAKY</code></a></h2>');
    expect(md('## See [spec](https://example.com/)', DOCS)).toBe('<h2 id="doc-see-spec">See <a href="https://example.com/" rel="nofollow noopener noreferrer ugc">spec</a></h2>');
    // An id outside anchorHref's plain grammar (a non-ASCII slug) gets no anchor rather than a throw.
    expect(md('## Größe', DOCS)).toBe('<h2 id="doc-größe">Größe</h2>');
  });

  // Mutant: code blocks left bare (no Copy button), or the box given an unnamed <pre>.
  test('a code block sits in a named copy box', () => {
    expect(md('```toml\n[package]\n```', DOCS)).toBe('<div class="code-box copy-box"><span class="code-lang" aria-hidden="true">toml</span><pre tabindex="0" role="region" aria-label="toml example"><code class="language-toml">[package]</code></pre></div>');
    expect(md('    plain', DOCS)).toBe('<div class="code-box copy-box"><span class="code-lang" aria-hidden="true">code</span><pre tabindex="0" role="region" aria-label="code example"><code>plain</code></pre></div>');
  });

  // Mutant: h1 or h4 reported, or the text carrying markup.
  test('onHeading hears the h2 and h3 headings, in order, as plain text', () => {
    md('# T\n## A `b`\n### C\n#### D\n## A `b`', DOCS);
    expect(heard).toEqual([[2, 'doc-a-b', 'A b'], [3, 'doc-c', 'C'], [2, 'doc-a-b-1', 'A b']]);
  });

  // The readme allowlist plus the code bar's <span>.
  test('the output stays inside the renderer allowlist', () => {
    const allowed = new Set([...MD_TAGS, 'span']);
    for (const [tag] of readTags(md('# T\n## A\n```\nx\n```\n| a |\n|---|\n| b |', DOCS)).tags) expect(allowed).toContain(tag);
  });
});
