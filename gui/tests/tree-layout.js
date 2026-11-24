"use strict";

const assert = require("node:assert/strict");
const { statusFixture, withDesktop } = require("./desktop-browser");

const fixture = statusFixture + `
  const paths = [".config/ghostty/config", ".config/hypr/.theme_state",
    ".config/hypr/conf/animations.conf", ".config/hypr/conf/animations_clean.conf", ".config/hypr/conf/themes/current.conf",
    ".config/hypr/conf/themes/overrides/local/current.conf",
    ...Array.from({ length: 146 }, (_, i) => ".config/hypr/conf/themes/overrides/local/set-" + i + ".conf")];
  window.fixtureStatus.entries = paths.map((key, i) => ({ ...window.fixtureStatus.entries[2], id: "clean-" + i, key }));
  window.fixtureStatus.clean = paths.length;
`;

async function check({ evaluate, waitFor, resize, key }) {
  await resize(1200, 800);
  await waitFor("document.querySelectorAll('#rows .row-name').length === 152");
  await evaluate("document.fonts.ready.then(() => document.querySelector('.tree-divider').focus())");
  for (const [dividerKey, width] of [["Home", 220], ["Enter", 320], ["End", 820]]) {
    await key(dividerKey);
    const layout = await evaluate(`(() => {
      const pane = document.getElementById("rows");
      const name = pane.querySelector('[title=".config/hypr/conf/animations.conf"] .row-name');
      const status = name.closest(".row").querySelector(".row-status");
      const siblingFolder = pane.querySelector('[data-folder-path=".config/hypr/conf/themes"] > .file-tree-branch .file-tree-label');
      const labels = pane.querySelectorAll(".row-name,.row-status,.file-tree-count");
      return {
        width: document.getElementById("entries").offsetWidth,
        scrollsDown: pane.scrollHeight > pane.clientHeight,
        fitsAcross: pane.scrollWidth === pane.clientWidth,
        labelsAligned: Math.abs(name.getBoundingClientRect().left - siblingFolder.getBoundingClientRect().left) < 1,
        namesFit: [...labels].every(label => label.scrollWidth <= label.clientWidth + 1),
        clearsScrollbar: [...labels].every(label =>
          label.getBoundingClientRect().right <= pane.getBoundingClientRect().left + pane.clientWidth - 16),
        inline: status.getBoundingClientRect().top < name.getBoundingClientRect().bottom &&
          name.getBoundingClientRect().top < status.getBoundingClientRect().bottom,
      };
    })()`);
    assert.deepEqual(layout, {
      width, scrollsDown: true, fitsAcross: true, labelsAligned: true, namesFit: true, clearsScrollbar: true, inline: width > 220,
    }, `Nested file names, statuses and counts at a ${width}px pane`);
  }
}

module.exports = () => withDesktop(fixture, check);
