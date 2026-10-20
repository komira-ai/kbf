# kbf.dev site inputs

Inputs for the kbf.dev site build, which is built on the same stack as mopm.dev. These files
are not a site build of their own: the build takes the theme as is and the pages' content
from here.

- `theme.css`, `THEME.md`: the shared theme for kbf.dev, mopm.dev and ucdk. Taken as is;
  the build pins its sha256.
- `index.html`, `quickstart.html`, `install.html`, `story.html`, `design.html`,
  `architecture.html`, `config.html`, `updates.html`: the pages' copy and structure. Page
  order: install and get started, then the demo, then why, then the rest.
- `story.js`, `story/`: the story page's scroll timeline, and the panel comparing a fixed
  7 GiB memory slot with the memory ladder on measured peak memory (`story/mem.json`).
- `demo.js`, `demo/`: the demo player, which replays a recorded build session on a test
  farm (`demo/trace.js`, `demo/cast.js`).
- `site.js`, `style.css`: the prototype's page script and layout, for reference.
