# Theme: one look for the komira open-source sites

One short spec, shared by mopm.dev, kbf.dev and ucdk.dev. The feel is mopm.dev's: warm
neutrals, an orange accent, system fonts, light and dark by `prefers-color-scheme`.

## The canonical file

`theme.css` is the theme. Each site copies it byte for byte and never edits its copy; a site's
own rules go in its own stylesheet, loaded after it (`style.css` here). A change to the theme is
made once, here, with the new digest below, and the other sites then take the new file.

theme.css sha256: `ea04dacad9e29d2b7e5cf0afe34c9631b05881f0cb82f9e23d1688926f5d1993`

kbf's `cargo xtask site check` fails when that line and the file disagree.

## Tokens

All are custom properties on `:root`, redefined under `prefers-color-scheme: dark`.

| Token | Use |
|---|---|
| `--bg`, `--bg-soft`, `--surface`, `--surface-2`, `--bg-code` | page, header and footer tint, cards, raised cards, code |
| `--fg`, `--fg-2`, `--fg-muted` | text, secondary text, quiet text |
| `--border`, `--border-strong`, `--control-border` | rules, strong rules, form controls (3:1 on the page) |
| `--accent`, `--accent-strong`, `--accent-soft`, `--accent-fg` | the one accent (orange), its hover, its tint, text on it |
| `--magenta` | the second hue, used only in the hero gradient and the step numbers |
| `--link`, `--focus` | links and focus rings (blue) |
| `--ok`, `--warn-*`, `--error-*` | status |
| `--radius`, `--radius-sm`, `--shadow`, `--shadow-lg`, `--wrap` | shape and width (76rem) |
| `--sans`, `--mono` | system stacks only: no web fonts, no CDNs |

## The mark

A prompt, then the site's name, in the mono face, bold, with the `>` in the accent colour:

```html
<a class="brand" href="index.html" aria-label="NAME home"><span class="gt" aria-hidden="true">&gt;</span>NAME</a>
```

It is text, so it needs no image and follows light and dark. mopm's "m" logo is not used.

## Page skeleton

```html
<a class="skip" href="#main">Skip to content</a>
<header class="site-header"><div class="wrap bar">
  <a class="brand" ...>   <nav class="site-nav" aria-label="Site"><ul><li><a href=...></a></li></ul></nav>
</div></header>
<main id="main" class="wrap"> ... </main>        <!-- the home page: <main id="main"> holding bands -->
<footer class="site-footer"><div class="wrap"><p>License line.</p><ul><li><a>Source</a></li></ul></div></footer>
```

Stylesheets in the head: `theme.css`, then the site's own. Scripts are `defer` files.
The current page's nav link has `aria-current="page"`.

## Section order of a home page

1. **Hero**: what it is in one sentence, the install command in a `.cmd` block, a primary button.
2. **Get started**: a `.stepper` of three steps, each with its command.
3. **Demo**: the product doing real work; a recording, never invented numbers.
4. **Why we built it**: a `.why-grid` of reasons.
5. **The rest**: architecture, modes, status, links to the docs.

Components for these: `.hero`, `.display`, `.eyebrow`, `.pill`, `.cmd` (with a copy button added
by script), `.band` and `.kicker`, `.stepper`, `.strip`, `.goals`, `.tag`.

## Status tags

Every claim about a feature carries one: `<span class="tag tag-ok">works today</span>` (it is in
the code) or `<span class="tag tag-planned">specified</span>` (decided or designed, not built).

## Rules every site keeps

- **No inline style or script, and no external resource.** The policy is
  `default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self'`: no `style=`
  attribute, no `<style>` or inline `<script>`, no `on*=` handler, no fetch (data a script needs is
  a script file). Timing and colour live in classes.
- **Light and dark** by `prefers-color-scheme`, **phone width** from 320px, and
  **`prefers-reduced-motion`**: motion is decoration and stops; nothing depends on it.
- Plain HTML, the CSS and a little script; built with Rust tooling, no Node.
