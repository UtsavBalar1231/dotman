"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { statusFixture, withDesktop } = require("./desktop-browser");

const evidence = process.env.DOTMAN_GUI_EVIDENCE;
const layoutCases = [[1000, 720], [1440, 900], [390, 600]];
const incoming = 'button[title=".config/helix/config.toml"]';

async function check({ page, evaluate, waitFor, resize, key }) {
  const visible = selector => `Boolean(document.querySelector(${JSON.stringify(selector)})?.getClientRects().length)`;
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  const renderedDiff = () => waitFor(visible("#diff-actions .review-action"));
  const fits = () => evaluate("document.documentElement.scrollWidth <= innerWidth");
  const selectTheme = theme => evaluate(`(() => {
    const select = document.querySelector("#theme-mode");
    select.value = ${JSON.stringify(theme)};
    select.dispatchEvent(new Event("change", { bubbles: true }));
  })()`);
  const capture = async name => {
    if (!evidence) return;
    fs.mkdirSync(evidence, { recursive: true });
    await evaluate("Promise.allSettled(document.getAnimations().map(animation => animation.finished)).then(() => true)");
    const { data } = await page("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(path.join(evidence, name + ".png"), Buffer.from(data, "base64"));
  };

  await waitFor(visible(incoming));
  assert.equal(await evaluate(`document.fonts.ready.then(() => {
    const family = getComputedStyle(document.querySelector("#search")).fontFamily;
    return family.includes("Departure Mono") && [...document.fonts].some(face =>
      face.family.includes("Departure Mono") && face.status === "loaded");
  })`), true, "The bundled Departure Mono face is loaded for controls");
  assert.equal(await evaluate("document.querySelector('#group-status').getAttribute('aria-pressed')"), "true");
  assert.equal(await evaluate("document.querySelector('button[title=plain]')"), null, "Changed excludes clean files");
  assert.deepEqual(await evaluate(`(() => {
    const row = document.querySelector("#rows .row");
    const name = row.querySelector(".row-name");
    return {
      truncated: name.scrollWidth > name.clientWidth && getComputedStyle(name).textOverflow === "ellipsis",
      fullName: row.getAttribute("aria-label").startsWith(row.title + " — "),
    };
  })()`), { truncated: true, fullName: true }, "Long file names keep their full accessible names");
  await click("#group-folders");
  assert.equal(await evaluate("Boolean(document.querySelector('.file-tree-branch use[href=\"#fluent-chevron-right\"]'))"), true);
  assert.equal(await evaluate("document.querySelectorAll('#rows [role=treeitem][tabindex=\"0\"]').length"), 1);
  assert.equal(await evaluate("document.querySelector('[role=tree]')?.getAttribute('aria-label')"), "All files");
  await evaluate("document.querySelector('#rows [tabindex=\"0\"]').focus()");
  await key("ArrowLeft");
  assert.equal(await evaluate("document.activeElement.getAttribute('aria-expanded')"), "false");
  await key("ArrowRight");
  assert.equal(await evaluate("document.activeElement.getAttribute('aria-expanded')"), "true");
  await key("ArrowRight");
  assert.equal(await evaluate("document.activeElement.parentElement.parentElement.className"), "file-tree-folder");
  await key("ArrowLeft");
  await key("ArrowLeft");
  assert.equal(await evaluate("document.activeElement.className"), "file-tree-folder");
  await key("ArrowDown");
  assert.equal(await evaluate("document.activeElement.getAttribute('role')"), "treeitem");
  await key("End");
  assert.equal(await evaluate("document.activeElement.title"), "plain");
  await key("Enter");
  await waitFor(visible("#diff"));
  await key("Escape");
  assert.equal(await evaluate("document.activeElement.title"), "plain");
  await key("f", 2);
  assert.equal(await evaluate("document.activeElement.id"), "search");
  await click("#group-status");
  await resize(360, 320);
  await click(incoming);
  await renderedDiff();
  assert.equal(await evaluate(`(() => {
    const bounds = document.querySelector("#diff-actions .review-action").getBoundingClientRect();
    return bounds.height >= 44 && bounds.top >= 0 && bounds.bottom <= innerHeight;
  })()`), true, "Replacement action fits in a short window");
  await click("#close-diff");

  for (const theme of ["light", "dark"]) {
    await click("#nav-settings");
    await selectTheme(theme);
    for (const [width, height] of layoutCases) {
      await resize(width, height);
      assert.equal(await fits(), true, `Settings ${width}px ${theme}`);
      await capture(`settings-${width}-${theme}`);
    }
    await click("#nav-files");
    for (const [width, height] of layoutCases) {
      await resize(width, height);
      assert.equal(await fits(), true, `Files ${width}px ${theme}`);
      await capture(`files-${width}-${theme}`);
      await click(incoming);
      await renderedDiff();
      assert.equal(await fits(), true, `Comparison ${width}px ${theme}`);
      if (width === 390) {
        assert.deepEqual(await evaluate(`(() => {
          const back = document.querySelector("#close-diff");
          const title = document.querySelector("#diff-key");
          return {
            backLabel: back.getAttribute("aria-label"),
            compact: back.getBoundingClientRect().width === 44,
            oneLine: title.getBoundingClientRect().height <= parseFloat(getComputedStyle(title).lineHeight) + 1,
          };
        })()`), { backLabel: "Back to files", compact: true, oneLine: true });
      }
      const geometry = await evaluate(`(() => {
        const diff = document.querySelector("#diff-text");
        const frame = document.querySelector(".comparison-reader").getBoundingClientRect();
        const action = document.querySelector("#diff-actions .review-action");
        const before = action.getBoundingClientRect();
        const topbar = document.querySelector(".topbar");
        diff.scrollTop = diff.scrollHeight;
        return {
          scrollable: diff.scrollHeight > diff.clientHeight && diff.scrollTop > 0,
          visible: before.height >= 44 && before.top >= topbar.getBoundingClientRect().bottom && before.bottom <= innerHeight,
          unmoved: action.getBoundingClientRect().top === before.top,
          aligned: Math.abs(frame.right - before.right) <= 1,
        };
      })()`);
      assert.deepEqual(geometry, { scrollable: true, visible: true, unmoved: true, aligned: true }, `Aligned, reachable action at ${width}px`);
      await capture(`diff-${width}-${theme}`);
      await click("#diff-actions .review-action");
      assert.equal(await evaluate(visible("#replacement-review")), true);
      assert.equal(await evaluate(visible("#diff-text")), false);
      assert.equal(await evaluate(visible("#close-diff")), false);
      assert.equal(await evaluate("document.querySelector('#replacement-batch').getBoundingClientRect().bottom <= innerHeight"), true);
      await capture(`replacement-${width}-${theme}`);
      await click("#replacement-back");
      await click("#close-diff");
    }
    await click("#nav-review");
    await waitFor(visible("#commit-message"));
    for (const [width, height] of layoutCases) {
      await resize(width, height);
      assert.equal(await fits(), true, `Review ${width}px ${theme}`);
      await capture(`review-${width}-${theme}`);
    }
    await click("#close-review");
  }

  await resize(1000, 720);
  await click(incoming);
  await renderedDiff();
  assert.equal(await evaluate(visible(".dr-options-list")), false);
  assert.equal(await evaluate("getComputedStyle(document.querySelector('.dr-options .disclosure-glyph')).transform"), "none");
  await evaluate("document.querySelector('.dr-options summary').focus()");
  await key(" ");
  assert.equal(await evaluate(visible(".dr-options-list")), true);
  assert.equal(await evaluate("getComputedStyle(document.querySelector('.dr-options .disclosure-glyph')).transform"), "matrix(0, 1, -1, 0, 0, 0)");
  await click('.dr-options-list [aria-expanded]');
  assert.equal(await evaluate(visible(".dr-header")), true);
  await evaluate("document.querySelector('.dr-options-list [aria-expanded]').focus()");
  await key("Escape");
  assert.equal(await evaluate(visible("#diff-text")), true);
  assert.equal(await evaluate(visible(".dr-options-list")), false);
  await evaluate("document.querySelector('#search').focus()");
  await resize(320, 450);
  assert.equal(await evaluate("document.activeElement.id"), "search");
  await resize(1000, 720);
  await evaluate("document.querySelector('#diff-text').focus()");
  await resize(320, 450);
  assert.equal(await evaluate("document.activeElement.id"), "diff-text");
  assert.equal(await evaluate(visible("#entries")), false);
  assert.equal(await fits(), true);
  await click("#close-diff");

  await evaluate('window.fixtureStatus.entries[1].key = "subdirectory/".repeat(200) + "test.toml"');
  await click("#refresh");
  const longPath = "#rows .row[title$='test.toml']";
  await waitFor(visible(longPath));
  await click(longPath);
  await renderedDiff();
  assert.deepEqual(await evaluate(`(() => {
    const title = document.querySelector(".comparison-title");
    return {
      pathCanScroll: title.scrollHeight > title.clientHeight && title.tabIndex === 0,
      actionOnscreen: document.querySelector(".review-action").getBoundingClientRect().bottom <= innerHeight,
      codeVisible: document.querySelector("#diff-text").clientHeight >= 96,
    };
  })()`), { pathCanScroll: true, actionOnscreen: true, codeVisible: true });
  await click("#close-diff");
  await evaluate('window.fixtureStatus.entries[1].key = ".config/helix/config.toml"');
  await click("#refresh");
  await waitFor(visible(incoming));

  await resize(1000, 720);
  await click(incoming);
  await renderedDiff();
  await page("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
  await click("#diff-actions .review-action");
  assert.equal(await evaluate("getComputedStyle(document.querySelector('#replacement-review')).animationName"), "none");
  await page("Emulation.setEmulatedMedia", { features: [] });
  await click("#replacement-batch");
  assert.equal(await evaluate(visible("#decisions")), false);
  assert.equal(await evaluate(visible("#diff-text")), true);
  assert.equal(await evaluate("window.fixtureCalls.some(call => call.command === 'apply_reviewed_files')"), false);
  await click("#review-decisions");
  assert.equal(await evaluate("document.activeElement.id"), "decisions-title");
  await capture("decision-queue-1000-dark");
  await key("f", 2);
  await evaluate("new Promise(resolve => requestAnimationFrame(() => resolve(true)))");
  assert.equal(await evaluate("document.activeElement.id"), "search");
  await click("#review-decisions");
  await click("#apply-decisions");
  await waitFor(visible("#retry-decisions"));
  assert.deepEqual(await evaluate("window.fixtureCalls.find(call => call.command === 'apply_reviewed_files').args.answers"), [
    { id: "1:1", action: "take_repo" },
  ]);
  await click("#retry-decisions");
  await waitFor(visible(incoming));
  assert.equal(await evaluate(visible("#decisions")), false);

  await click("#nav-review");
  await waitFor(visible("#commit-message"));
  assert.equal(await evaluate("document.querySelector('#review-items h3').textContent.startsWith('Hypothetical effects')"), true);
  await evaluate(`(() => {
    const draft = document.querySelector("#commit-message");
    draft.value = "Edited draft";
    draft.dispatchEvent(new Event("input", { bubbles: true }));
    draft.focus();
  })()`);
  for (const width of [1000, 320, 895, 2560]) {
    await resize(width, 450);
    assert.deepEqual(await evaluate(`({
      focus: document.activeElement.id,
      draft: document.querySelector("#commit-message").value,
      fits: document.documentElement.scrollWidth <= innerWidth,
    })`), { focus: "commit-message", draft: "Edited draft", fits: true }, `Review ${width}px`);
  }
  await click("#close-review");

  await evaluate('document.documentElement.style.fontSize = "200%"');
  await resize(1000, 450);
  assert.equal(await fits(), true, "Files at 200% text");
  await evaluate("document.documentElement.style.removeProperty('font-size')");
  await evaluate(`window.fixtureStatus.entries = Array.from({ length: 1200 }, (_, index) => ({
    id: "scale:" + index, key: "file-" + index, status: "changed-locally", group: "outgoing",
    supported: true, system: false,
  }));`);
  await click("#refresh");
  await waitFor("Boolean(document.querySelector('#rows .row'))");
  await evaluate(`(() => {
    const search = document.querySelector("#search");
    search.value = "file-1199";
    search.dispatchEvent(new Event("input", { bubbles: true }));
  })()`);
  await evaluate("new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve(true))))");
  assert.deepEqual(await evaluate("Array.from(document.querySelectorAll('#rows .row'), row => row.title)"), ["file-1199"]);
  await evaluate(`(() => {
    const search = document.querySelector("#search");
    search.value = "";
    search.dispatchEvent(new Event("input", { bubbles: true }));
  })()`);
  await waitFor("Boolean(document.querySelector('#rows .row[title=file-1199]'))");
  assert.equal(await evaluate("document.querySelectorAll('#rows .row').length"), 1200);
  await evaluate(`window.fixtureStatus = { ...window.fixtureStatus, success: false, finding: "unavailable", repo_unavailable: true, entries: [] }`);
  await click("#refresh");
  await waitFor(visible("#setup"));
  for (const theme of ["light", "dark"]) {
    await selectTheme(theme);
    for (const [width, height] of layoutCases) {
      await resize(width, height);
      assert.equal(await fits(), true, `Unavailable ${width}px ${theme}`);
      await capture(`unavailable-${width}-${theme}`);
    }
  }
  await evaluate(`window.fixtureStatus = { ...window.fixtureStatus, success: true, finding: "available", repo_unavailable: false };
    window.recoveryFixture = true;`);
  await click("#refresh");
  await waitFor(visible("#entries"));
  await click("#nav-review");
  await waitFor(visible("#recovery"));
  for (const theme of ["light", "dark"]) {
    await selectTheme(theme);
    for (const [width, height] of layoutCases) {
      await resize(width, height);
      assert.equal(await fits(), true, `Recovery ${width}px ${theme}`);
      await capture(`recovery-${width}-${theme}`);
    }
  }
  assert.deepEqual(await evaluate("window.violations"), [], "production bundle under restrictive policy");
  console.log("Workbench layout, keyboard, decision queue, recovery, and CSP passed");
}

withDesktop(statusFixture, check).catch(error => {
  console.error(error);
  process.exitCode = 1;
});
