"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const withBrowser = require("./browser");

const dist = path.join(__dirname, "../dist");
const mock = `
const long = "configuration-with-a-very-long-unbroken-name-".repeat(4);
window.__TAURI__ = { core: { Channel: class {}, invoke: async (command, args) => {
  if (command === "refresh") return window.fixtureStatus;
  if (command === "open_diff") return { outcome: "ready", success: true, message: "Ready", file_actions: args.id === "1:1" ? ["take_repo"] : [], text: "diff --git repo/file home/file\\n--- repo/file\\n+++ home/file\\n@@ -1 +1 @@\\n-" + long + "\\n+" + long + "changed\\n" };
  if (command === "open_review") return window.recoveryFixture ? { outcome: "terminal_recovery", message: long, items: [] } : { id: "preview", outcome: "review", message: "Ready", items: [{ group: "Outgoing", text: long }], commit_message: "draft" };
  if (command === "discard_review") return {};
  throw Error("Unexpected bridge call: " + command);
} } };
window.fixtureStatus = { success: true, message: "Status loaded", key_stripped: false, clean: 0, remote: "local",
  context: { home: "/home/test", repo: "/repo/" + long, config: "/config", state: "/state", identity: "/identity", machine: "test" },
  entries: [{ id: "1:0", key: "folder/" + long, status: "changed-locally", group: "outgoing", supported: true, system: false },
    { id: "1:1", key: ".config/helix/config.toml", status: "changed-in-repo", group: "incoming", supported: true, system: false }] };
`;

async function run() {
  const server = http.createServer((req, res) => {
    const name = req.url === "/" ? "index.html" : req.url.slice(1);
    if (!/^[\w.-]+$/.test(name)) { res.writeHead(404).end(); return; }
    try {
      let body = fs.readFileSync(path.join(dist, name));
      if (name === "index.html") body = Buffer.from(body.toString().replace('<script type="module"', `<script>${mock}</script><script type="module"`));
      const type = name.endsWith(".js") ? "text/javascript" : name.endsWith(".css") ? "text/css" : name.endsWith(".html") ? "text/html" : "font/woff";
      res.writeHead(200, { "Content-Type": type }).end(body);
    } catch { res.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  try {
    await withBrowser(async send => {
    const { targetId } = await send("Target.createTarget", { url: "about:blank" });
    const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
    const page = (method, params) => send(method, params, sessionId);
    const evalJs = async expression => {
      const { result, exceptionDetails } = await page("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
      assert.ok(!exceptionDetails, exceptionDetails?.exception?.description ?? exceptionDetails?.text);
      return result.value;
    };
    await page("Page.enable");
    await page("Page.navigate", { url: `http://127.0.0.1:${server.address().port}/` });
    await evalJs(`new Promise((resolve, reject) => {
      const end = Date.now() + 5000;
      const check = () => document.querySelector(".row") ? resolve() : Date.now() > end ? reject(Error("status did not load")) : setTimeout(check, 20);
      check();
    })`);
    const resize = async (width, height, scale = 1) => {
      await page("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
      await evalJs(`document.documentElement.style.fontSize = "${112.5 * scale}%"`);
    };
    const runJs = code => evalJs(`(async () => { ${code} })()`);
    await resize(1000, 700);
    await runJs(`document.querySelector('button[title=".config/helix/config.toml"]').click()`);
    await evalJs(`new Promise(resolve => requestAnimationFrame(resolve))`);
    for (const width of [1220, 896, 390, 320]) {
      await resize(width, 740);
      const geometry = await runJs(`const b = document.querySelector("#diff-actions .review-action");
        const box = b.getBoundingClientRect(), glyph = b.querySelector("svg").getBoundingClientRect();
        const range = document.createRange(); range.selectNodeContents(b.firstChild);
        const label = range.getBoundingClientRect();
        return [box.height >= 44, Math.round(label.left - box.left), Math.round(box.right - glyph.right),
          Math.round(glyph.left - label.right), Math.round(box.right - document.querySelector("#diff-text").getBoundingClientRect().right)];`);
      assert.deepEqual(geometry, [true,16,16,12,0], `Replacement action at ${width}px`);
      await runJs(`document.querySelector("#diff-actions .review-action").click()`);
      assert.equal(await runJs(`return !document.querySelector("#replacement-review").hidden && document.querySelector("#decisions").hidden
        && !document.querySelector("#diff-text").getClientRects().length && document.documentElement.scrollWidth <= document.documentElement.clientWidth`), true, `Read-only review at ${width}px`);
      await runJs(`document.querySelector("#replacement-back").click()`);
    }
    await runJs(`document.querySelector("#close-diff").click()`);
    await resize(1000, 700);
    await runJs(`document.querySelector("#search").value = "configuration"; document.querySelector("#search").dispatchEvent(new Event("input", { bubbles: true })); document.querySelector(".row").click(); await new Promise(r => setTimeout(r, 50)); document.querySelector("#search").focus();`);
    for (const [width, height, zoom] of [[1000,700,1], [897,700,1], [896,700,1], [895,450,1], [320,450,1], [1000,450,2], [2560,900,1], [1000,700,1]]) {
      await resize(width, height, zoom);
      const state = await runJs(`const search = document.querySelector("#search"), diff = document.querySelector("#diff");
        return { focus: document.activeElement === search, search: search.value, visible: search.getClientRects().length > 0,
          diff: !diff.hidden, fits: document.documentElement.scrollWidth <= document.documentElement.clientWidth };`);
      assert.deepEqual(state, { focus: true, search: "configuration", visible: true, diff: true, fits: true }, `Files and diff ${width}x${height} scale ${zoom}`);
    }
    await runJs(`document.querySelector(".row").focus()`);
    await resize(320, 450);
    assert.equal(await runJs(`const row = document.querySelector(".row"); return document.activeElement === row && row.getClientRects().length > 0`), true, "focused queue row survives reflow");
    await runJs(`document.querySelector("#diff-text").focus()`);
    assert.equal(await runJs(`return document.querySelector("#entries").getClientRects().length`), 0, "narrow diff hides an unfocused navigator");
    await resize(1800, 700);
    await runJs(`document.querySelector(".dr-toggle").focus()`);
    await resize(320, 450);
    assert.equal(await runJs(`const b = document.querySelector(".dr-toggle"); return document.activeElement === b && b.getClientRects().length > 0`), true, "focused diff view control survives reflow");
    await runJs(`document.querySelector("#nav-review").click(); await new Promise(r => setTimeout(r, 50)); const t = document.querySelector("#commit-message"); t.value = "edited draft"; t.focus();`);
    for (const width of [1000, 320, 895, 2560]) {
      await resize(width, 450);
      const state = await runJs(`const t = document.querySelector("#commit-message"); return { focus: document.activeElement === t, value: t.value, visible: t.getClientRects().length > 0, fits: document.documentElement.scrollWidth <= document.documentElement.clientWidth };`);
      assert.deepEqual(state, { focus: true, value: "edited draft", visible: true, fits: true }, `Review ${width}px`);
    }
    await runJs(`document.querySelector("#nav-settings").click(); await new Promise(r => setTimeout(r, 50)); document.querySelector("#policy-controls").open = true; document.querySelector("#policy-controls summary").focus();`);
    for (const width of [1000, 320, 896, 2560]) {
      await resize(width, 450);
      const state = await runJs(`const b = document.querySelector("#policy-controls summary"), action = document.querySelector("#policy-ignores"); action.scrollIntoView(); return { focus: document.activeElement === b, open: document.querySelector("#policy-controls").open, visible: b.getClientRects().length > 0, fits: document.documentElement.scrollWidth <= document.documentElement.clientWidth, reachable: action.getBoundingClientRect().bottom <= innerHeight };`);
      assert.deepEqual(state, { focus: true, open: true, visible: true, fits: true, reachable: true }, `Settings ${width}px`);
    }
    await runJs(`window.fixtureStatus = { ...window.fixtureStatus, success: false, repo_unavailable: true, entries: [] }; document.querySelector("#refresh").click(); await new Promise(r => setTimeout(r, 50)); document.querySelector("#nav-files").click();`);
    for (const width of [320, 1000]) {
      await resize(width, 450);
      const state = await runJs(`const setup = document.querySelector("#setup"); setup.scrollIntoView(); return { visible: !setup.hidden, fits: document.documentElement.scrollWidth <= document.documentElement.clientWidth };`);
      assert.deepEqual(state, { visible: true, fits: true }, `Unavailable ${width}px`);
    }
    await runJs(`window.fixtureStatus = { ...window.fixtureStatus, success: true, repo_unavailable: false, entries: [{ id: "1:0", key: "folder/file", status: "changed-locally", group: "outgoing", supported: true, system: false }] }; document.querySelector("#refresh").click(); await new Promise(r => setTimeout(r, 50)); window.recoveryFixture = true; document.querySelector("#nav-review").click(); await new Promise(r => setTimeout(r, 50));`);
    for (const width of [320, 1000]) {
      await resize(width, 450);
      const state = await runJs(`const recovery = document.querySelector("#recovery"); recovery.scrollIntoView(); return { visible: !recovery.hidden, fits: document.documentElement.scrollWidth <= document.documentElement.clientWidth };`);
      assert.deepEqual(state, { visible: true, fits: true }, `Recovery ${width}px`);
    }
    console.log("Live resize: Files/diff, review draft, Settings, unavailable and recovery passed; 200% root text");
    });
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
}

run().catch(error => { console.error(error); process.exitCode = 1; });
