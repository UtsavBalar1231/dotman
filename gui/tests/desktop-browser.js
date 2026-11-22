"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const withBrowser = require("./browser");

const dist = path.resolve(process.env.DOTMAN_GUI_DIST ?? path.join(__dirname, "../dist"));
const csp = "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src ipc: http://ipc.localhost; img-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-src 'none'";
const contentTypes = new Map([
  [".html", "text/html; charset=utf-8"], [".js", "text/javascript; charset=utf-8"],
  [".css", "text/css; charset=utf-8"], [".woff", "font/woff"], [".woff2", "font/woff2"],
  [".txt", "text/plain; charset=utf-8"],
]);

function readCss() {
  return fs.readdirSync(path.join(dist, "assets"))
    .filter(name => name.endsWith(".css")).sort()
    .map(name => fs.readFileSync(path.join(dist, "assets", name), "utf8"))
    .join("\n");
}

async function withDesktop(fixture, check) {
  const server = http.createServer((request, response) => {
    const name = request.url === "/" ? "index.html" : request.url?.slice(1);
    if (!name || !/^(?:assets\/)?[\w.-]+$/.test(name)) {
      response.writeHead(404).end();
      return;
    }
    try {
      let body;
      if (name === "fixture.js") {
        body = `window.violations = [];
document.addEventListener("securitypolicyviolation", event => window.violations.push(event.effectiveDirective + " " + event.blockedURI));
${fixture}`;
      } else {
        body = fs.readFileSync(path.join(dist, name));
        if (name === "index.html") body = body.toString("utf8").replace("<head>", '<head><script src="/fixture.js"></script>');
      }
      response.writeHead(200, {
        "Content-Type": contentTypes.get(path.extname(name)) ?? "application/octet-stream",
        "Content-Security-Policy": csp,
        "X-Content-Type-Options": "nosniff",
      }).end(body);
    } catch (error) {
      if (!(error instanceof Error) || error.code !== "ENOENT") throw error;
      response.writeHead(404).end();
    }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  try {
    await withBrowser(async send => {
      const { targetId } = await send("Target.createTarget", { url: "about:blank" });
      const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
      const page = (method, params) => send(method, params, sessionId);
      const evaluate = async expression => {
        const { result, exceptionDetails } = await page("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
        assert.ok(!exceptionDetails, exceptionDetails?.exception?.description ?? exceptionDetails?.text);
        return result.value;
      };
      const waitFor = expression => evaluate(`new Promise((resolve, reject) => {
        const done = () => {
          if (!(${expression})) return;
          observer.disconnect();
          clearTimeout(timeout);
          resolve(true);
        };
        const observer = new MutationObserver(done);
        observer.observe(document, { attributes: true, childList: true, subtree: true, characterData: true });
        const timeout = setTimeout(() => {
          observer.disconnect();
          reject(new Error("Desktop condition was not reached"));
        }, 5000);
        done();
      })`);
      const resize = (width, height) => page("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
      const key = async (key, modifiers = 0) => {
        const windowsVirtualKeyCode = { " ": 32, Enter: 13, Escape: 27, End: 35, ArrowLeft: 37, ArrowRight: 39, ArrowDown: 40, f: 70 }[key];
        const code = key === " " ? "Space" : key.length === 1 ? `Key${key.toUpperCase()}` : key;
        const event = { key, code, modifiers, windowsVirtualKeyCode };
        await page("Input.dispatchKeyEvent", { type: "rawKeyDown", ...event });
        await page("Input.dispatchKeyEvent", { type: "keyUp", ...event });
      };
      await page("Page.enable");
      await page("Page.bringToFront");
      await page("Page.navigate", { url: `http://127.0.0.1:${server.address().port}/` });
      await check({ page, evaluate, waitFor, resize, key });
    });
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
}

const statusFixture = `
const longPath = "configuration-with-a-very-long-unbroken-name-".repeat(4);
window.fixtureStatus = {
  success: true, finding: "available", message: "Status loaded", key_stripped: false, clean: 1, remote: "local",
  context: { home: "/home/test", repo: "/repo/" + longPath, config: "/config", state: "/state", identity: "/identity", machine: "test" },
  entries: [
    { id: "1:0", key: ".config/niri/" + longPath, status: "changed-locally", group: "outgoing", supported: true, system: false },
    { id: "1:1", key: ".config/helix/config.toml", status: "changed-in-repo", group: "incoming", supported: true, system: false, editable: true },
    { id: "1:2", key: "plain", status: "clean", group: "clean", supported: true, system: false },
  ],
};
const patch = "diff --git repo/file home/file\\n--- repo/file\\n+++ home/file\\n@@ -1,34 +1,34 @@\\n-old\\n+new\\n" +
  Array.from({ length: 32 }, (_, i) => " context" + i + "\\n").join("") + "-end\\n+next\\n";
window.fixtureCalls = [];
window.__TAURI__ = { core: { Channel: class {}, invoke: async (command, args) => {
  window.fixtureCalls.push({ command, args });
  if (command === "refresh") return window.fixtureStatus;
  if (command === "open_diff") return { outcome: "ready", success: true, message: "different", file_actions: args.id === "1:1" ? ["take_repo"] : [], text: patch };
  if (command === "open_review") return window.recoveryFixture ? { outcome: "terminal_recovery", message: longPath, items: [] } : {
    id: "preview", outcome: "review", message: "Ready",
    items: [{ group: "Already-ahead commits", text: "Earlier commit" }, { group: "Hypothetical effects", text: longPath }],
    commit_message: "draft",
  };
  if (command === "apply_reviewed_files") return { outcome: "declined", message: "Apply declined.", items: [], refresh: false };
  if (command === "discard_review") return {};
  throw Error("Unexpected bridge call: " + command);
} } };
`;

module.exports = { readCss, withDesktop, statusFixture };
