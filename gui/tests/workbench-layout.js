"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { statusFixture, withDesktop } = require("./desktop-browser");
const checkTreeLayout = require("./tree-layout");

const evidence = process.env.DOTMAN_GUI_EVIDENCE;
const layoutCases = [[1000, 720], [1440, 900], [390, 600]];
const incoming = 'button[title=".config/helix/config.toml"]';

async function check({ page, evaluate, waitFor, resize, key }) {
  const visible = selector => `Boolean(document.querySelector(${JSON.stringify(selector)})?.getClientRects().length)`;
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  const renderedDiff = () => waitFor(visible("#diff-actions .review-action"));
  const fits = () => evaluate("document.documentElement.scrollWidth <= innerWidth");
  const selectTheme = theme => click(`input[name=theme][value="${theme}"]`);
  const capture = async name => {
    if (!evidence) return;
    fs.mkdirSync(evidence, { recursive: true });
    await evaluate("Promise.allSettled(document.getAnimations().map(animation => animation.finished)).then(() => true)");
    const { data } = await page("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(path.join(evidence, name + ".png"), Buffer.from(data, "base64"));
  };

  await waitFor(visible("#rows [role=tree]"));
  await resize(1440, 900);
  const brand = await evaluate(`(() => {
    const rect = document.getElementById("nav-files").getBoundingClientRect();
    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
  })()`);
  await page("Input.dispatchMouseEvent", { type: "mouseMoved", ...brand });
  await waitFor("document.querySelector('#brand-hint')?.textContent === 'File browser'");
  await key("Escape");
  await waitFor("!document.getElementById('brand-hint')");
  await click("#nav-settings");
  await page("Input.dispatchMouseEvent", { type: "mouseMoved", x: 320, y: 250 });
  await page("Input.dispatchMouseEvent", { type: "mouseMoved", ...brand });
  await waitFor("document.querySelector('#brand-hint')?.textContent === 'Back to files'");
  await click("#nav-files");
  await waitFor("document.body.dataset.page === 'files' && !document.getElementById('brand-hint')");
  await page("Input.dispatchMouseEvent", { type: "mouseMoved", x: 320, y: 250 });
  await evaluate("document.getElementById('refresh').focus()");
  await key("Tab", 8);
  assert.equal(await evaluate("document.activeElement.id"), "nav-files");
  await waitFor("document.getElementById('nav-files').matches(':focus-visible') && document.getElementById('brand-hint') !== null");
  await key("Escape");
  await waitFor("!document.getElementById('brand-hint') && document.activeElement.id === 'nav-files'");
  await waitFor("document.querySelector('.tree-divider').ariaValueMax === '1060'");
  const point = await evaluate(`(() => {
    const box = document.querySelector(".tree-divider").getBoundingClientRect();
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  })()`);
  await page("Input.dispatchMouseEvent", { type: "mousePressed", ...point, button: "left", clickCount: 1 });
  await page("Input.dispatchMouseEvent", { type: "mouseMoved", x: 418, y: point.y, buttons: 1 });
  await page("Input.dispatchMouseEvent", { type: "mouseReleased", x: 418, y: point.y, button: "left", clickCount: 1 });
  await waitFor("document.querySelector('#entries').offsetWidth === 418");
  await key("ArrowRight");
  assert.equal(await evaluate("document.querySelector('.tree-divider').ariaValueNow"), "434");
  await page("Page.reload");
  await waitFor("document.querySelector('#entries')?.offsetWidth === 434");
  await evaluate("document.querySelector('.tree-divider').focus()");
  await key("ArrowLeft");
  await key("Home");
  assert.equal(await evaluate("document.querySelector('#entries').offsetWidth"), 220);
  await key("End");
  assert.equal(await evaluate("document.querySelector('#entries').offsetWidth"), 1060);
  await key("Enter");
  assert.equal(await evaluate("document.querySelector('#entries').offsetWidth"), 320);
  await click("#group-status");
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

  const reachable = selector => evaluate(`(() => {
    const button = document.querySelector(${JSON.stringify(selector)});
    const box = button.getBoundingClientRect();
    return box.top >= 0 && box.bottom <= innerHeight &&
      button.contains(document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2));
  })()`);
  for (const [width, remove] of [[320, "#clear-decisions"], [360, "#decision-items button"], [320, "#clear-decisions"]]) {
    await resize(width, 320);
    await click(incoming);
    await renderedDiff();
    await click("#diff-actions .review-action");
    assert.equal(await reachable("#replacement-batch"), true, `Replacement fits at ${width} × 320`);
    await click("#replacement-batch");
    await waitFor(visible("#review-decisions"));
    await evaluate("new Promise(resolve => requestAnimationFrame(() => resolve(true)))");
    assert.equal(await reachable("#diff-actions .review-action"), true, "Queued comparison action fits");
    assert.equal(await evaluate(`(() => {
      const body = document.querySelector("#comparison-body").getBoundingClientRect();
      const text = document.querySelector("#diff-text").getBoundingClientRect();
      return Math.min(body.bottom, text.bottom) - Math.max(body.top, text.top) >= 80;
    })()`), true, "Queueing a replacement returns to the diff text");
    await click("#review-decisions");
    assert.equal(await evaluate(visible("#diff")), false, "The open queue owns the short workspace");
    await evaluate('document.querySelector("#apply-decisions").focus()');
    assert.equal(await reachable("#apply-decisions"), true, "The queue scrolls its action into view");
    await key("Escape");
    await evaluate("new Promise(resolve => requestAnimationFrame(() => resolve(true)))");
    assert.equal(await evaluate("document.activeElement.id"), "review-decisions");
    await click("#close-diff");
    await click('button[title^=".config/niri/"]');
    assert.equal(await evaluate("document.activeElement.id"), "close-diff");
    assert.equal(await evaluate("document.querySelector('#comparison-body').scrollTop"), 0, "A different file opens at its heading");
    await click("#review-decisions");
    await click(remove);
    await key("Escape");
    assert.equal(await evaluate(visible(incoming)), true, "Escape returns to Files after removing the last decision");
  }

  await click("#nav-settings");
  await evaluate("document.querySelector('#context summary').focus()");
  await key(" ");
  for (const [width, height, scale] of [[1715, 1373, 100], [3440, 1440, 100], [609, 450, 200], [320, 420, 200]]) {
    await resize(width, height);
    await evaluate(`document.documentElement.style.fontSize = "${scale}%"`);
    assert.deepEqual(await evaluate(`(() => {
      const settings = document.querySelector("#settings");
      const path = document.querySelector("#repository-path");
      const labels = [...document.querySelectorAll(".theme-picker label")];
      return {
        readablePath: path.clientWidth >= Math.min(300, settings.clientWidth * .65),
        fits: settings.scrollWidth <= settings.clientWidth,
        expanded: document.querySelector("#context").open,
        focused: document.activeElement.matches("#context summary"),
        themesFit: innerWidth < 1000 || labels.every(label => label.offsetTop === labels[0].offsetTop),
      };
    })()`), { readablePath: true, fits: true, expanded: true, focused: true, themesFit: true },
    `Expanded Settings at ${width}×${height}, ${scale}% text`);
  }
  await key(" ");
  await evaluate("document.documentElement.style.removeProperty('font-size')");
  await click("#nav-files");

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
          const group = document.querySelector(".comparison-title");
          return {
            backLabel: back.getAttribute("aria-label"),
            compact: back.getBoundingClientRect().width === 44,
            oneLine: title.getBoundingClientRect().height <= parseFloat(getComputedStyle(title).lineHeight) + 1,
            statusFits: group.scrollHeight === group.clientHeight,
            editVisible: document.querySelector("#source-edit-details button").getBoundingClientRect().height >= 44,
          };
        })()`), { backLabel: "Back to files", compact: true, oneLine: true, statusFits: true, editVisible: true });
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
    await waitFor(visible("#commit-editor summary"));
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
  await waitFor(visible("#commit-editor summary"));
  assert.equal(await evaluate("document.querySelector('#commit-editor').open"), true);
  await resize(1440, 900);
  assert.equal(await evaluate(`(() => {
    const drawer = document.querySelector("#review");
    const box = drawer.getBoundingClientRect();
    return drawer.matches(":modal") && box.right === innerWidth && box.width < innerWidth * .6;
  })()`), true, "Review is docked to the right of the workspace");
  assert.equal(await evaluate("document.querySelector('#review-items summary').textContent.startsWith('Planned changes')"), true);
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
  await waitFor("document.querySelector('#discard-review-work').matches(':modal')");
  assert.equal(await evaluate("document.activeElement.id"), "keep-review-work");
  await resize(320, 450);
  assert.equal(await evaluate("document.getElementById('discard-review-work').scrollWidth <= innerWidth"), true);
  await key("Escape");
  assert.equal(await evaluate("document.querySelector('#review').matches(':modal')"), true);
  assert.equal(await evaluate("document.getElementById('commit-message').value"), "Edited draft");
  assert.equal(await evaluate("document.activeElement.id"), "commit-message");
  await click("#restore-commit-message");
  assert.equal(await evaluate("document.activeElement.id"), "commit-message");
  assert.equal(await evaluate("document.getElementById('commit-message').value"), "draft");
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

async function checkSourceEditor({ page, evaluate, waitFor, key, resize }) {
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  const shadow = "document.querySelector('#source-buffer').shadowRoot";
  const code = `Array.from(${shadow}.querySelectorAll('.cm-line'), line => line.textContent).join('\\n')`;
  const insert = text => page("Input.insertText", { text });
  await waitFor("document.querySelector('[role=treeitem][title=plain]')");
  await click("[role=treeitem][title=plain]");
  await waitFor("document.querySelector('#source-edit-details button:enabled')");
  await click("#source-edit-details button");
  await waitFor(`${shadow}?.activeElement?.matches('.cm-content')`);
  assert.equal(await evaluate(code), "initial\n");
  await key("a", 2);
  await insert("changed\n");
  await click(".source-undo");
  assert.equal(await evaluate(code), "initial\n");
  await click(".source-redo");
  assert.equal(await evaluate(code), "changed\n");
  await click(".source-find");
  await evaluate(`${shadow}.querySelector('[aria-label=Find]').focus()`);
  await insert("changed");
  assert.equal(await evaluate(`${shadow}.querySelector('output').textContent`), "1 match");
  await evaluate(`${shadow}.querySelector('button[aria-label="Whole word"]').click()`);
  assert.equal(await evaluate(`${shadow}.querySelector('button[aria-label="Whole word"]').getAttribute("aria-pressed")`), "true");
  await evaluate(`${shadow}.querySelector('button[aria-label=Replace]').click()`);
  await insert("replaced");
  await resize(360, 320);
  assert.deepEqual(await evaluate(`(() => {
    const root = ${shadow};
    const panel = root.querySelector(".cm-panels-top");
    const code = root.querySelector(".cm-scroller");
    const reachable = (selector, scrollTop) => {
      panel.scrollTop = scrollTop;
      const control = root.querySelector(selector);
      const bounds = control.getBoundingClientRect();
      return root.elementFromPoint(bounds.x + bounds.width / 2, bounds.y + bounds.height / 2) === control;
    };
    return {
      scrollable: panel.scrollHeight > panel.clientHeight && getComputedStyle(panel).overflowY === "auto",
      codeVisible: code.clientHeight >= parseFloat(getComputedStyle(code).lineHeight),
      find: reachable("input[aria-label=Find]", 0),
      replaceAll: reachable("button[aria-label='Replace all']", panel.scrollHeight),
    };
  })()`), { scrollable: true, codeVisible: true, find: true, replaceAll: true }, "Expanded find and replace preserves the editor and both ends of the controls in a short window");
  await evaluate(`const replaceAll = ${shadow}.querySelector('button[aria-label="Replace all"]'); replaceAll.focus(); replaceAll.click()`);
  assert.equal(await evaluate(code), "replaced\n");
  await key("Escape");
  assert.equal(await evaluate("document.querySelector('#source-editor').open"), true);
  assert.equal(await evaluate(`Boolean(${shadow}.querySelector('.source-search'))`), false);
  await click(".source-position");
  await evaluate(`${shadow}.querySelector('input[name=line]').select()`);
  await insert("2");
  await key("Enter");
  await waitFor("document.querySelector('.source-position').textContent === 'Ln 2, Col 1'");
  await click(".source-wrap");
  assert.equal(await evaluate(`Boolean(${shadow}.querySelector('.cm-lineWrapping'))`), true);
  await resize(360, 430);
  assert.equal(await evaluate("document.querySelector('#source-editor').scrollWidth <= innerWidth"), true);
  await key("Escape");
  await waitFor("document.querySelector('.source-exit').hidden === false");
  assert.equal(await evaluate("document.querySelector('#source-editor').matches(':modal')"), true);
  assert.equal(await evaluate("document.activeElement.className"), "source-continue");
  await key("Escape");
  await waitFor(`${shadow}.activeElement?.matches('.cm-content')`);
  assert.equal(await evaluate(code), "replaced\n");
  await evaluate("window.requestQuit()");
  assert.equal(await evaluate("window.quitCount"), 0);
  assert.equal(await evaluate("document.activeElement.className"), "source-continue");
  assert.equal(await evaluate("document.querySelector('.source-discard').textContent"), "Discard and quit");
  assert.equal(await evaluate("document.querySelector('.source-save-exit').textContent"), "Save and quit…");
  await click(".source-continue");
  await waitFor(`${shadow}.activeElement?.matches('.cm-content')`);
  await evaluate("window.requestQuit()");
  await click(".source-save-exit");
  await waitFor("document.querySelector('#source-editor [role=status]').textContent.includes('File changed')");
  assert.equal(await evaluate(code), "replaced\n");
  assert.equal(await evaluate("window.copiedDraft"), undefined);
  await click(".source-copy");
  assert.equal(await evaluate("window.copiedDraft"), "replaced\r\n");
  await evaluate(`${shadow}.querySelector('.cm-content').focus(); window.failSave = false`);
  await key("s", 2);
  await waitFor("!document.querySelector('#source-editor').open && document.querySelector('#source-edit-details button:enabled')");
  assert.equal(await evaluate("window.quitCount"), 0);
  assert.equal(await evaluate("window.requestQuit"), undefined);
  assert.equal(await evaluate("window.sourceText"), "replaced\r\n");
  await click("#source-edit-details button");
  await waitFor(`${shadow}.activeElement?.matches('.cm-content')`);
  assert.equal(await evaluate(code), "replaced\n");
  assert.equal(await evaluate("document.querySelector('.source-undo').disabled"), true);
  await key("Escape");
  await evaluate(`window.fixtureStatus.entries[2].key = "settings.ts";
    window.sourceText = "const width: number = 80;";`);
  await click("#refresh");
  const script = '[role=treeitem][title="settings.ts"]';
  await waitFor(`document.querySelector(${JSON.stringify(script)})`);
  await click(script);
  await waitFor("document.querySelector('#source-edit-details button:enabled')");
  await click("#source-edit-details button");
  await waitFor("document.querySelector('.source-language').textContent === 'TypeScript'");
  assert.equal(await evaluate(code), "const width: number = 80;");
  await key("a", 2);
  await insert("discard me");
  await click(".source-cancel");
  await click(".source-discard");
  assert.equal(await evaluate("window.sourceText"), "const width: number = 80;");
  assert.equal(await evaluate(`${shadow}.childElementCount`), 0);
  await click("#source-edit-details button");
  await waitFor(`${shadow}.activeElement?.matches('.cm-content')`);
  await key("a", 2);
  await insert("confirmed draft");
  await evaluate("window.holdSave = true; window.requestQuit()");
  await click(".source-save-exit");
  await evaluate("window.requestQuit()");
  assert.equal(await evaluate("window.quitCount"), 0, "Closing the window during confirmation keeps the draft open");
  await evaluate("window.confirmSave()");
  await waitFor("window.quitCount === 1");
  assert.equal(await evaluate("window.sourceText"), "confirmed draft");
  assert.deepEqual(await evaluate("window.violations"), []);
}

async function checkSyncReceipt({ evaluate, waitFor, resize, key }) {
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  await waitFor("document.querySelector('#rows .row')");
  await click("#nav-review");
  await waitFor("!document.querySelector('#sync-reviewed').hidden");
  await click("#sync-reviewed");
  await waitFor("document.querySelector('#review-title').textContent === 'Sync complete' && !document.querySelector('#review-return').disabled");
  assert.deepEqual(await evaluate(`Array.from(document.querySelectorAll("#review-items summary"), row => [row.firstChild.textContent, row.parentElement.open])`),
    [["File changes", true], ["Hook activity", false], ["Sync activity", false]]);
  assert.equal(await evaluate("document.querySelector('.sync-effect').textContent"), "captured");
  assert.equal(await evaluate("document.querySelector('#review-remote').hidden"), true);
  await resize(360, 420);
  await evaluate("document.querySelector('.sync-activity summary').focus()");
  assert.equal(await evaluate("document.activeElement.matches('.sync-activity summary')"), true, "Activity disclosure receives keyboard focus");
  await key(" ");
  await waitFor("document.querySelector('.sync-activity[open]')");
  assert.equal(await evaluate("document.querySelector('.sync-activity[open] li').textContent"), "Capture local changes");
  assert.equal(await evaluate("document.querySelector('#review').scrollWidth <= innerWidth"), true);
  await click("#review-return");
  assert.equal(await evaluate("document.querySelector('#review').open"), false);
  assert.deepEqual(await evaluate("window.violations"), []);
}

async function checkTracking({ evaluate, waitFor, resize, key }) {
  await waitFor("document.querySelector('#rows [role=tree]')");
  await evaluate("document.getElementById('group-status').click()");
  const parents = await evaluate("Array.from(document.querySelectorAll('#rows .row-directory')).filter(el => el.getClientRects().length).map(el => el.textContent)");
  assert.ok(parents.includes(".config/helix/") && parents.includes(".config/niri/"));
  assert.equal(await evaluate("document.querySelector('#rows .row-status').hidden"), true);
  for (const [width, height, theme] of [[1440, 900, "light"], [390, 580, "dark"], [640, 340, "light"]]) {
    await resize(width, height);
    await evaluate(`document.documentElement.dataset.theme = "${theme}"; document.querySelector('#track-menu summary').focus()`);
    await key("ArrowDown");
    await key("ArrowDown");
    assert.equal(await evaluate("document.activeElement.id"), "track-folder");
    await key(" ");
    await waitFor("document.querySelector('#tracking-drawer[open]')");
    assert.equal(await evaluate("document.querySelector('.tracking-reason').textContent"), "Protected filename");
    assert.equal(await evaluate("document.querySelector('.tracking-holds .row-name').textContent"), ".env");
    const layout = await evaluate(`(() => {
      const dialog = document.getElementById("tracking-drawer");
      const body = dialog.querySelector(".tracking-body");
      body.scrollTop = body.scrollHeight;
      const action = document.getElementById("tracking-apply").getBoundingClientRect();
      return body.scrollWidth <= body.clientWidth && action.bottom <= innerHeight && action.left >= 0;
    })()`);
    assert.ok(layout, `tracking review fits ${width} × ${height}`);
    await key("Escape");
    await waitFor("!document.querySelector('#tracking-drawer').open && document.activeElement.matches('#track-menu summary')");
  }
  await evaluate("window.heldOnly = true; document.querySelector('#track-menu summary').click(); document.getElementById('track-files').click()");
  await waitFor("document.getElementById('tracking-drawer').open");
  assert.equal(await evaluate("document.querySelector('#tracking-drawer footer').getClientRects().length"), 0);
  assert.match(await evaluate("document.getElementById('tracking-scope').textContent"), /no new files/i);
  await key("Escape");
  await evaluate("window.heldOnly = false; document.querySelector('#track-menu summary').click(); document.getElementById('track-folder').click()");
  await waitFor("document.getElementById('tracking-drawer').open");
  const bulk = ".tracking-bulk input";
  const filter = ".tracking-choices input[type=search]";
  const setFilter = value => evaluate(`(() => {const input = document.querySelector('${filter}'); input.value = ${JSON.stringify(value)}; input.dispatchEvent(new Event('input'));})()`);
  await evaluate(`document.querySelector('${bulk}').click()`);
  assert.equal(await evaluate("document.getElementById('tracking-apply').disabled"), true);
  await setFilter("name-17");
  await evaluate(`document.querySelector('${bulk}').click()`);
  await setFilter("");
  assert.equal(await evaluate(`document.querySelector('${bulk}').indeterminate`), true);
  assert.equal(await evaluate("document.getElementById('tracking-count-label').textContent"), "1 of 18 selected");
  assert.match(await evaluate("document.getElementById('tracking-scope').textContent"), /No new folder capture rule/);
  await evaluate("document.getElementById('tracking-future-input').click()");
  assert.match(await evaluate("document.getElementById('tracking-scope').textContent"), /Unchecked paths will be ignored/);
  await evaluate("document.getElementById('tracking-apply').click()");
  await waitFor("!document.getElementById('tracking-drawer').open");
  assert.deepEqual(await evaluate("window.trackingSelection"), {excluded: Array.from({length:17}, (_, i) => i), keep_folder_rule:true});
}

async function checkStatusIssue({ evaluate, waitFor, resize }) {
  const click = id => evaluate(`document.getElementById(${JSON.stringify(id)}).click()`);
  await waitFor("document.querySelector('#status-issue:not([hidden]) #status-issue-title')?.textContent.includes('Reading')");
  assert.equal(await evaluate("document.getElementById('retry-status').hidden"), true);
  await evaluate("window.finishInitialRead()");
  await waitFor("!document.getElementById('open-installation').hidden");
  assert.equal(await evaluate("document.getElementById('entries').hidden"), true);
  assert.equal(await evaluate("document.getElementById('setup').hidden"), true);
  for (const [width, height] of [[1280,800], [390,640], [320,320]]) {
    await resize(width, height);
    assert.equal(await evaluate("document.documentElement.scrollWidth <= innerWidth"), true);
    assert.equal(await evaluate("document.getElementById('retry-status').getClientRects().length"), 1);
  }
  await click("open-installation");
  await waitFor("document.getElementById('installation-feedback').textContent");
  assert.deepEqual(await evaluate("window.lastFolder"), {folder:"installation"});
  await evaluate("window.readIssue = 'timeout'; document.getElementById('retry-status').focus()");
  await click("retry-status");
  await waitFor("document.getElementById('status-issue-title').textContent.includes('timed out')");
  assert.equal(await evaluate("document.getElementById('open-installation').hidden"), true);
  assert.equal(await evaluate("document.activeElement.id"), "retry-status");
  await evaluate("window.readIssue = null");
  await click("retry-status");
  await waitFor("!document.getElementById('entries').hidden");
  assert.equal(await evaluate("document.getElementById('status-issue').hidden"), true);
  assert.equal(await evaluate("document.activeElement.id"), "search");
}

async function checkRefreshOnReturn({ evaluate, waitFor }) {
  const count = () => evaluate("window.fixtureCalls.filter(call => call.command === 'refresh').length");
  const returnToWindow = () => evaluate("window.dispatchEvent(new Event('blur')); window.dispatchEvent(new Event('focus'))");
  const refresh = async () => {
    await evaluate("window.beforeRead = document.querySelector('#rows [role=tree]')");
    await returnToWindow();
    await waitFor("window.beforeRead !== document.querySelector('#rows [role=tree]')");
  };
  await waitFor("document.querySelector('#rows [role=tree]')");
  assert.equal(await count(), 1, "fresh launch reads status");
  await evaluate(`document.querySelector('#rows [title=plain]').focus();
    window.fixtureStatus.entries[2].id = "new-generation";
    window.fixtureStatus.entries[2].status = "changed-locally";
    window.fixtureStatus.entries[2].group = "outgoing";`);
  await refresh();
  assert.equal(await evaluate("document.activeElement.title"), "plain", "Returning keeps the same file across fresh status IDs");
  assert.match(await evaluate("document.activeElement.ariaLabel"), /Local change/);
  await evaluate("document.querySelector('[data-folder-path]').focus()");
  const folder = await evaluate("document.activeElement.dataset.folderPath");
  await refresh();
  assert.equal(await evaluate("document.activeElement.dataset.folderPath"), folder);
  assert.equal(await evaluate("document.getElementById('untrack-folder').title"), folder);
  await evaluate("document.querySelector('#rows [title=plain]').focus(); window.fixtureStatus.entries[2].key = 'external-edit'");
  await refresh();
  assert.equal(await evaluate("document.activeElement.id"), "search", "A removed file returns focus to search");
  await evaluate("window.dispatchEvent(new Event('focus'))");
  assert.equal(await count(), 4, "one read per return");
  await evaluate("document.querySelector('#rows [title=external-edit]').click()");
  await waitFor("!document.getElementById('diff').hidden && !document.getElementById('refresh').disabled");
  await returnToWindow();
  assert.equal(await count(), 4, "an open comparison survives returning to the app");
  assert.equal(await evaluate("document.getElementById('diff').hidden"), false);
}

async function checkComparisons({ evaluate, waitFor, resize, key }) {
  const read = selector => `document.querySelector(${JSON.stringify(selector)})`;
  const click = selector => evaluate(`${read(selector)}.click()`);
  await resize(1200, 780);
  await waitFor("document.querySelectorAll('#attention-list button').length === 6");
  await click("#more-comparisons");
  assert.equal(await evaluate("document.activeElement.textContent.includes('app6/')"), true);
  await click("#attention-list li:last-child button");
  await waitFor("document.querySelector('#diff-actions button')?.textContent.includes('Keep')");
  await click("#diff-actions button");
  await click("#close-diff");
  assert.equal(await evaluate("document.activeElement === document.querySelector('#attention-list li:last-child button')"), true);
  assert.equal(await evaluate(`${read("#guide-description")}.textContent`), "7 to inspect · 1 queued for review");
  assert.match(await evaluate(`${read("#attention-list li:last-child button")}.ariaLabel`), /app7.*Queued/);
  assert.ok(await evaluate(`${read("#more-comparisons")}.hidden`));
  await resize(390, 600);
  assert.ok(await evaluate("document.querySelector('.desk-guide').getClientRects().length === 0"));
  const search = async (value, count) => {
    await evaluate(`{ const field = document.getElementById("search"); field.focus(); field.value = ${JSON.stringify(value)}; field.dispatchEvent(new Event("input")); }`);
    assert.equal(await evaluate(`${read("#search-count")}.textContent`), count);
  };
  await search("queued", "1 match");
  await search("both copies changed", "8 matches");
  await click("#group-status");
  assert.equal(await evaluate(`${read("#search-count")}.textContent`), "8 matches");
  await search("app7", "1 match");
  assert.equal(await evaluate("document.querySelector('#rows .row-status').textContent"), "Queued");
  assert.match(await evaluate("document.querySelector('#rows .row').ariaLabel"), /Queued.*Keep/);
  await key("Escape");
  assert.equal(await evaluate(`${read("#search")}.value`), "");
  assert.equal(await evaluate("document.activeElement.id"), "search");
  await search("none", "0 matches");
  await click("#clear-search");
  assert.equal(await evaluate("document.activeElement.id"), "search");
  assert.equal(await evaluate(`${read("#search-count")}.textContent`), "");
  assert.equal(await evaluate("document.documentElement.scrollWidth <= innerWidth"), true);
}

async function checkHeldFile({ evaluate, waitFor, resize, key }) {
  await waitFor("document.querySelector('.held-row')");
  await evaluate("window.delayHeldRead = true; window.dispatchEvent(new Event('blur')); window.dispatchEvent(new Event('focus'))");
  await evaluate("document.querySelector('.held-row').closest('[role=treeitem]').focus()");
  await key("Enter");
  await waitFor("document.querySelector('.held-actions button')");
  await evaluate("window.delayHeldRead = false; window.finishHeldRead(window.fixtureStatus)");
  await waitFor("!document.getElementById('refresh').disabled && document.querySelector('.held-row[open] button')");
  for (const width of [1200, 390, 320]) {
    await resize(width, 660);
    assert.equal(await evaluate(`(() => {
      const body = document.querySelector('.held-details');
      return body.scrollWidth <= body.clientWidth && Array.from(body.querySelectorAll('button, dd'))
        .every(element => element.getBoundingClientRect().right <= body.getBoundingClientRect().right + 1);
    })()`), true);
  }
  await key("Tab");
  assert.equal(await evaluate("document.activeElement.textContent"), "Encrypt…");
  assert.equal(await evaluate("window.heldRequests"), 0);
  await key(" ");
  await waitFor("document.getElementById('held-message').textContent === 'Review cancelled.'");
  assert.equal(await evaluate("document.getElementById('held-result').getClientRects().length"), 1);
  assert.equal(await evaluate("window.heldRequests"), 1);
  await evaluate("document.getElementById('dismiss-held').click()");
  assert.equal(await evaluate("document.activeElement.id"), "search");
}

async function checkDestination({ evaluate, waitFor, resize }) {
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  await waitFor("document.querySelector('#rows [role=tree]')");
  await resize(390,640);
  await click("#open-destination");
  await waitFor("document.activeElement === document.querySelector('#destination-settings summary')");
  await waitFor("document.querySelectorAll('.destination-link').length === 2");
  await click(".destination-link:last-of-type[aria-label='Open example/fork on GitHub']");
  assert.equal(await evaluate("window.openedRepository"), "example/fork");
  await click("#destination-settings button:not(.destination-link)");
  await waitFor("document.activeElement === document.querySelector('.destination-form input')");
  assert.deepEqual(await evaluate("Array.from(document.querySelectorAll('.destination-form input:not([type=checkbox])'), e => e.value)"),
    ["https://github.com:443/upstream/config.git","ssh://git@ssh.github.com:443/example/fork.git"]);
  assert.equal(await evaluate("document.querySelector('.destination-form button[type=submit]').disabled"), true);
  await click(".destination-form input[type=checkbox]");
  await click(".destination-form button[type=submit]");
  await waitFor("document.querySelector('#destination-settings [role=status]')?.textContent === 'Destination saved.'");
  assert.deepEqual(await evaluate("window.savedChange"), {fetch:"https://github.com:443/upstream/config.git",push:null});
  assert.equal(await evaluate("document.querySelectorAll('.destination-link').length"),1);
  assert.equal(await evaluate("document.documentElement.scrollWidth <= innerWidth"),true);
  await click("#nav-files");
  assert.equal(await evaluate("document.getElementById('remote').textContent"), "Matched at fetch");
  assert.match(await evaluate("document.getElementById('open-destination').title"), /0 ahead · 0 behind/);
}

async function checkWideWorkbench({ evaluate, waitFor, resize, key }) {
  await waitFor("document.querySelector('#rows [role=tree]')");
  const click = selector => evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
  const geometry = [];
  for (const [width, height, treeWidth, controlHeight] of [
    [1440, 900, 320, 44], [1920, 1080, 400, 47], [3440, 1374, 480, 52], [390, 640, 390, 44],
  ]) {
    await resize(width, height);
    await waitFor(`document.getElementById('entries').offsetWidth === ${treeWidth}`);
    await click("#track-menu summary");
    const metrics = await evaluate(`(() => {
      const rect = selector => document.querySelector(selector).getBoundingClientRect();
      const font = selector => parseFloat(getComputedStyle(document.querySelector(selector)).fontSize);
      return {
        width: innerWidth, tree: rect('#entries').width, refresh: rect('#refresh').height,
        menuItem: rect('#track-files').height, menuFits: rect('.track-options').left >= 0,
        treeFont: font('#rows .row-name'), folderIcon: rect('.file-tree-name .workspace-glyph:not(.disclosure-glyph)').height,
        folderHeight: rect('.file-tree-branch').height, searchHeight: rect('#search').height, titleFont: font('#guide-title'), diffFont: font('#diff-text'),
        horizontalOverflow: document.documentElement.scrollWidth > innerWidth,
      };
    })()`);
    assert.equal(Math.round(metrics.refresh), controlHeight);
    assert.equal(Math.round(metrics.menuItem), controlHeight);
    assert.equal(metrics.menuFits, true);
    assert.equal(Math.round(metrics.searchHeight), controlHeight);
    assert.ok(metrics.folderIcon <= metrics.treeFont * 1.2);
    assert.equal(metrics.folderHeight, 40);
    assert.equal(metrics.horizontalOverflow, false);
    if (geometry.length) assert.equal(metrics.treeFont, geometry[0].treeFont);
    await click("#track-menu summary");
    await click("#nav-settings");
    metrics.settings = await evaluate("document.getElementById('settings').offsetWidth");
    await click("#nav-files");
    await click("#nav-review");
    await waitFor("document.getElementById('review').matches(':modal')");
    metrics.review = await evaluate("document.getElementById('review').offsetWidth");
    await key("Escape");
    await waitFor("!document.getElementById('review').open");
    geometry.push(metrics);
  }
  assert.deepEqual(geometry.map(row => row.settings), [1024, 1280, 1536, 390]);
  assert.deepEqual(geometry.map(row => row.review), [624, 780, 936, 390]);
  assert.ok(Math.abs(geometry[2].titleFont / geometry[0].titleFont - 1.25) < .001);
  assert.ok(Math.abs(geometry[2].diffFont / geometry[0].diffFont - 1.5) < .001);
  await resize(1920, 1080);
  await waitFor("document.getElementById('entries').offsetWidth === 400");
  await evaluate("document.querySelector('.tree-divider').focus()");
  await key("ArrowRight");
  await resize(3440, 1374);
  await waitFor("document.querySelector('.tree-divider').ariaValueMax === '3060'");
  assert.equal(await evaluate("document.getElementById('entries').offsetWidth"), 416);
  await key("Enter");
  assert.equal(await evaluate("document.getElementById('entries').offsetWidth"), 480);
  assert.equal(await evaluate("localStorage.getItem('dotman.treeWidth')"), null);
  await key("Home");
  await click("#track-menu summary");
  assert.equal(await evaluate(`(() => {
    const pane = document.getElementById("entries");
    const menu = document.querySelector(".track-options").getBoundingClientRect();
    return pane.scrollWidth === pane.clientWidth && menu.left >= 0 && menu.right <= pane.offsetWidth;
  })()`), true, "Toolbar and menu fit a manually narrowed ultrawide sidebar");
  if (evidence) {
    fs.mkdirSync(evidence, { recursive: true });
    fs.writeFileSync(path.join(evidence, "wide-geometry.json"), JSON.stringify(geometry, null, 2));
  }
}

async function checkCleanBrowser({ evaluate, waitFor, resize, key }) {
  await resize(1000, 720);
  await waitFor("document.querySelector('#rows [role=tree]')");
  await waitFor("document.querySelector('#entries').offsetWidth === 320");
  assert.equal(await evaluate("document.querySelector('#guide-title').textContent"), "152 files match the repository");
  await evaluate("document.fonts.ready.then(() => document.querySelector('#group-status').click())");
  assert.equal(await evaluate("document.querySelector('.file-list-empty button').textContent"), "Browse all files");
  assert.equal(await evaluate(`(() => {
    const label = document.querySelector('#remote').getBoundingClientRect();
    const clean = document.querySelector('#clean').getBoundingClientRect();
    return label.bottom > clean.top && clean.bottom > label.top && label.right < clean.left;
  })()`), true, "The last fetch and clean count share a line in the default 320px tree");
  await evaluate("document.querySelector('.tree-divider').focus()");
  await key("Home");
  assert.equal(await evaluate(`(() => {
    const pane = document.querySelector('#entries');
    return pane.offsetWidth === 220 && pane.scrollWidth === pane.clientWidth;
  })()`), true, "The sidebar fits at its minimum width");
  await evaluate("document.querySelector('.file-list-empty button').focus()");
  await key(" ");
  assert.equal(await evaluate("document.activeElement.id"), "group-folders");
  assert.equal(await evaluate("document.querySelectorAll('#rows [role=treeitem]').length"), 152);
  await evaluate(`(() => {
    const search = document.querySelector('#search');
    search.value = 'absent';
    search.dispatchEvent(new Event('input'));
  })()`);
  assert.equal(await evaluate("document.querySelector('.file-list-empty button').textContent"), "Clear search");
  await evaluate("document.querySelector('.file-list-empty button').click()");
  assert.equal(await evaluate("document.activeElement.id"), "search");
  assert.equal(await evaluate("document.querySelectorAll('#rows [role=treeitem]').length"), 152);
}

async function main() {
  await withDesktop(statusFixture, checkWideWorkbench);
  await checkTreeLayout();
  await withDesktop(statusFixture + `
    window.fixtureStatus.remote = "Last fetched: 0 ahead · 0 behind";
    window.fixtureStatus.clean = 152;
    window.fixtureStatus.entries = Array.from({length: 152}, (_, i) => ({
      ...window.fixtureStatus.entries[2], id: "clean-" + i, key: "file-" + i
    }));
  `, checkCleanBrowser);
  await withDesktop(statusFixture + `
    const invokeStatus = window.__TAURI__.core.invoke;
    window.readIssue = "invalid_report";
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "open_workspace_folder") { window.lastFolder = args; throw Error("private launch error"); }
      if (command === "refresh" && !window.finishInitialRead) await new Promise(resolve => { window.finishInitialRead = resolve; });
      if (command === "refresh" && window.readIssue) return {
        ...window.fixtureStatus, success:false, finding:window.readIssue, entries:[], context:null,
      };
      return invokeStatus(command, args);
    };
  `, checkStatusIssue);

  await withDesktop(statusFixture + `
    window.fixtureStatus.entries = [{ id:"held", key:".config/app/file", group:"held", status:"held-in-home",
      held_actions:["encrypt","forget"], hold:{ origin:"home_edit", reason:"Guard finding", rule:"github-fine-grained-pat", location:"line 51" }}];
    window.heldRequests = 0;
    const original = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = (command, args) => {
      if (command === "refresh" && window.delayHeldRead) return new Promise(resolve => { window.finishHeldRead = resolve; });
      if (command !== "run_held") return original(command, args);
      window.heldRequests++;
      return Promise.resolve({outcome:"declined",message:"Review cancelled.",refresh:true});
    };
  `, checkHeldFile);
  await withDesktop(statusFixture + `
    const prior = window.__TAURI__.core.invoke;
    const state = {token:"initial", branch:"main", fetch:["https://github.com:443/upstream/config.git"],
      push:["ssh://git@ssh.github.com:443/example/fork.git"], editable:true,separate_push:true};
    window.__TAURI__.core.invoke = async (command,args) => {
      if (command === "open_github_repository") { window.openedRepository = args.repository; return; }
      if (command !== "sync_destination") return prior(command,args);
      if (args.change) {
        if (args.expected !== state.token) throw Error("Wrong destination review");
        window.savedChange = args.change;
        window.fixtureStatus.remote = "Last fetched: 0 ahead · 0 behind";
        return {...state, token:"next", push:state.fetch, separate_push:false};
      }
      return state;
    };
  `, checkDestination);
  await withDesktop(statusFixture + `
    window.fixtureStatus.entries = Array.from({length: 8}, (_, i) => ({ id: "decision-" + i,
      key: ".config/app" + i + "/config.toml", status: "both-changed", group: "decision", supported: true }));
    const previousInvoke = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = (command, args) => command === "open_diff"
      ? Promise.resolve({outcome:"ready",success:true,message:"different",text:patch,file_actions:["keep_local"]})
      : previousInvoke(command, args);
  `, checkComparisons);
  await withDesktop(statusFixture + `
    window.fixtureStatus.entries[0].key = ".config/niri/config.toml";
    const invoke = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = async (command, args) => command === "apply_tracking"
      ? (window.trackingSelection = args.selection, true) : command === "preview_tracking" ?
      ["test-review", { folders: window.heldOnly ? [] : [".config/app"], files: window.heldOnly ? [] : Array.from({ length: 18 }, (_, i) => ".config/app/" + "long-name-".repeat(12) + i),
        existing: [], held: { ".config/app/.env": "Protected filename" } }] : invoke(command, args);
  `, checkTracking);
  await withDesktop(statusFixture, checkRefreshOnReturn);
  await withDesktop(statusFixture, check);
  await withDesktop(statusFixture + `
    const original = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = (command, args) => command !== "run_reviewed_sync" ? original(command, args) : Promise.resolve({
      outcome: "done", message: "Sync completed.", refresh: true,
      items: [{ group: "Reached stages", text: "capture" }, { group: "Changes", text: "a · b · captured" }, { group: "Hooks", text: "deferred" }],
    });
  `, checkSyncReceipt);
  await withDesktop(statusFixture + `
    window.fixtureStatus.entries[2].editable = true;
    window.sourceText = "initial\\r\\n";
    Object.defineProperty(navigator, "clipboard", {value:{writeText: async text => { window.copiedDraft = text; }}});
    window.quitCount = 0;
    window.__TAURI__.window = {getCurrentWindow: () => ({
      onCloseRequested: async handler => {
        window.requestQuit = () => {
          let prevented = false;
          handler({preventDefault() { prevented = true; }});
          if (!prevented) window.quitCount++;
        };
        return () => { delete window.requestQuit; };
      },
      destroy: async () => { window.quitCount++; },
    })};
    window.failSave = true;
    const originalInvoke = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "open_source") return window.sourceText;
      if (command === "save_source") {
        if (window.holdSave) await new Promise(resolve => { window.confirmSave = resolve; });
        if (window.failSave) throw Error("File changed");
        window.sourceText = args.text;
        return;
      }
      return originalInvoke(command, args);
    };
  `, checkSourceEditor);
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
