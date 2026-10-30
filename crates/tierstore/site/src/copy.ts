// Copy buttons for command blocks (.cmd and .code). Progressive enhancement: the pages read the same without it.
// No inline style, no markup from strings.

function addCopyButton(block: Element): void {
  const pre = block.querySelector('pre');
  if (!pre) return;
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'copy';
  button.textContent = 'Copy';
  button.addEventListener('click', () => {
    // The commands, not the prompt marks or the comments.
    const clone = pre.cloneNode(true) as HTMLElement;
    for (const n of clone.querySelectorAll('.p, .c, .prompt')) n.remove();
    const text = (clone.textContent ?? '').replace(/^[ \t]+/gm, '').replace(/\n{2,}/g, '\n').trim() + '\n';
    navigator.clipboard.writeText(text).then(
      () => {
        button.textContent = 'Copied';
        button.classList.add('done');
        setTimeout(() => {
          button.textContent = 'Copy';
          button.classList.remove('done');
        }, 1500);
      },
      () => {
        button.textContent = 'Press Ctrl+C';
      },
    );
  });
  block.appendChild(button);
}

if (navigator.clipboard) {
  for (const block of document.querySelectorAll('.cmd, .code')) addCopyButton(block);
}
