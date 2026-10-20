// Copy buttons for command blocks (.cmd and .code). Progressive enhancement: the pages read
// the same without it. No inline style, no markup from strings.
(function () {
  "use strict";
  if (!navigator.clipboard) { return; }
  var blocks = document.querySelectorAll(".cmd, .code");
  Array.prototype.forEach.call(blocks, function (block) {
    var pre = block.querySelector("pre");
    if (!pre) { return; }
    var button = document.createElement("button");
    button.type = "button";
    button.className = "copy";
    button.textContent = "Copy";
    button.addEventListener("click", function () {
      // The commands, not the prompt marks or the comments.
      var clone = pre.cloneNode(true);
      Array.prototype.forEach.call(clone.querySelectorAll(".p, .c, .prompt"), function (n) { n.remove(); });
      var text = clone.textContent.replace(/^[ \t]+/gm, "").replace(/\n{2,}/g, "\n").trim() + "\n";
      navigator.clipboard.writeText(text).then(
        function () {
          button.textContent = "Copied";
          button.classList.add("done");
          setTimeout(function () { button.textContent = "Copy"; button.classList.remove("done"); }, 1500);
        },
        function () { button.textContent = "Press Ctrl+C"; }
      );
    });
    block.appendChild(button);
  });
})();
