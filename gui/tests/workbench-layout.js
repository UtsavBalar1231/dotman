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
  await waitFor("document.querySelector('.source-discard').hidden === false");
  assert.equal(await evaluate("document.querySelector('#source-editor').matches(':modal')"), true);
  await click(".source-save");
  await waitFor("document.querySelector('#source-editor [role=status]').textContent.includes('File changed')");
  assert.equal(await evaluate(code), "replaced\n");
  await evaluate("window.failSave = false");
  await key("s", 2);
  await waitFor("!document.querySelector('#source-editor').open && document.querySelector('#source-edit-details button:enabled')");
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

async function main() {
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
    window.failSave = true;
    const originalInvoke = window.__TAURI__.core.invoke;
    window.__TAURI__.core.invoke = async (command, args) => {
      if (command === "open_source") return window.sourceText;
      if (command === "save_source") {
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
