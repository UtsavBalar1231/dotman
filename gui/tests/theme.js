"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const css = fs.readFileSync(path.join(__dirname, "../dist/style.css"), "utf8");
const html = `<!doctype html><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<style>${css}</style><div id="tab-start" tabindex="-1"></div><button>Focus</button><select><option>Theme</option></select>`;

function prepare(theme, accent) {
  const { dataset } = document.documentElement;
  if (theme === "system") delete dataset.theme; else dataset.theme = theme;
  if (accent === "default") delete dataset.accent; else dataset.accent = accent;
  const start = document.querySelector("#tab-start");
  start.focus();
  return document.activeElement === start;
}

function measure() {
  function rgb(value) {
    const probe = document.createElement("span");
    probe.style.color = value;
    document.body.append(probe);
    const parts = getComputedStyle(probe).color.match(/[\d.]+/g).slice(0, 3).map(Number);
    probe.remove();
    return parts.map(n => { n /= 255; return n <= .04045 ? n / 12.92 : ((n + .055) / 1.055) ** 2.4; });
  }
  function contrast(a, b) {
    const luminance = color => rgb(color).reduce((sum, n, i) => sum + n * [.2126, .7152, .0722][i], 0);
    const values = [luminance(a), luminance(b)].sort((x, y) => y - x);
    return (values[0] + .05) / (values[1] + .05);
  }
  const failures = [], floors = { text: Infinity, controls: Infinity };
  const tokens = getComputedStyle(document.documentElement);
  const value = name => tokens.getPropertyValue("--" + name).trim();
  for (const [foreground, background, category, minimum] of [
    ["ink", "bg", "text", 4.5], ["ink", "paper", "text", 4.5],
    ["ink", "surface", "text", 4.5], ["ink", "selected", "text", 4.5],
    ["muted", "paper", "text", 4.5], ["muted", "surface", "text", 4.5],
    ["blue", "paper", "text", 4.5], ["amber", "paper", "text", 4.5],
    ["green", "paper", "text", 4.5], ["blue", "selected", "text", 4.5],
    ["amber", "selected", "text", 4.5], ["green", "selected", "text", 4.5],
    ["ink", "add-bg", "text", 4.5], ["ink", "del-bg", "text", 4.5],
    ["added", "add-bg", "text", 4.5],
    ["removed", "del-bg", "text", 4.5], ["accent", "surface", "text", 4.5],
    ["accent", "selected", "text", 4.5], ["rule", "surface", "controls", 3],
    ["accent", "bg", "controls", 3], ["accent", "paper", "controls", 3]
  ]) {
    const ratio = contrast(value(foreground), value(background));
    floors[category] = Math.min(floors[category], ratio);
    if (ratio < minimum) failures.push(foreground + "/" + background + ": " + ratio.toFixed(2));
  }
  const button = document.querySelector("button");
  const focus = getComputedStyle(button);
  if (document.activeElement !== button) failures.push("Tab did not focus the button");
  if (!button.matches(":focus-visible")) failures.push("button did not match :focus-visible after Tab");
  if (focus.outlineStyle === "none" || focus.outlineStyle === "hidden" || !(parseFloat(focus.outlineWidth) >= 2)) {
    failures.push("missing keyboard focus outline: " + focus.outlineWidth + " " + focus.outlineStyle);
  }
  const select = getComputedStyle(document.querySelector("select"));
  if (select.borderStyle === "none" || parseFloat(select.borderWidth) < 1) failures.push("missing select border");
  return { failures, floors, dark: matchMedia("(prefers-color-scheme: dark)").matches };
}

async function checkThemes(send) {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const page = (method, params) => send(method, params, sessionId);
  await page("Page.bringToFront");
  const { frameTree } = await page("Page.getFrameTree");
  await page("Page.setDocumentContent", { frameId: frameTree.frame.id, html });
  async function evaluate(expression) {
    const { result, exceptionDetails } = await page("Runtime.evaluate", { expression, returnByValue: true });
    assert.ok(!exceptionDetails, exceptionDetails?.exception?.description ?? exceptionDetails?.text);
    return result.value;
  }

  for (const dark of [false, true]) {
    const preference = dark ? "dark" : "light";
    await page("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: preference }] });
    const floors = { text: Infinity, controls: Infinity };
    const failures = [];
    for (const theme of ["system", "light", "dark"]) for (const accent of ["default", "marine", "mulberry"]) {
      const label = `system ${preference}, ${theme}/${accent}`;
      assert.equal(await evaluate(`(${prepare})(${JSON.stringify(theme)}, ${JSON.stringify(accent)})`), true,
        `${label}: could not reset focus before Tab`);
      const tab = { key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 };
      await page("Input.dispatchKeyEvent", { type: "rawKeyDown", ...tab });
      await page("Input.dispatchKeyEvent", { type: "keyUp", ...tab });
      const report = await evaluate(`(${measure})()`);
      assert.equal(report.dark, dark, `${label}: system color scheme did not match test mode`);
      failures.push(...report.failures.map(failure => `${label}: ${failure}`));
      for (const category of Object.keys(floors)) floors[category] = Math.min(floors[category], report.floors[category]);
    }
    assert.deepEqual(failures, []);
    console.log(`system ${preference}: text floor ${floors.text.toFixed(2)}, controls floor ${floors.controls.toFixed(2)}`);
  }
}

async function run() {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), "dt-"));
  try {
    const browser = spawn(process.env.CHROMIUM || "chromium", [
      "--headless", "--disable-gpu", "--disable-background-networking", "--disable-component-update",
      "--no-first-run", "--no-proxy-server", "--host-resolver-rules=MAP * ~NOTFOUND",
      `--user-data-dir=${path.join(temp, "profile")}`, "--remote-debugging-pipe", "about:blank"
    ], {
      stdio: ["ignore", "ignore", "pipe", "pipe", "pipe"],
      env: { PATH: process.env.PATH, HOME: temp, TMPDIR: temp, XDG_CONFIG_HOME: path.join(temp, "config"),
        XDG_CACHE_HOME: path.join(temp, "cache"), XDG_DATA_HOME: path.join(temp, "data"),
        XDG_STATE_HOME: path.join(temp, "state"), XDG_RUNTIME_DIR: temp,
        DBUS_SESSION_BUS_ADDRESS: `unix:path=${path.join(temp, "no-bus")}`,
        DBUS_SYSTEM_BUS_ADDRESS: `unix:path=${path.join(temp, "no-bus")}` }
    });
    const input = browser.stdio[3], output = browser.stdio[4];
    let pending, failure, closing = false, id = 0, buffer = "", stderr = "";
    function fail(error) {
      failure ??= error;
      pending?.reject(failure);
      pending = undefined;
    }
    browser.once("error", fail);
    browser.stderr.setEncoding("utf8").on("data", chunk => { stderr = (stderr + chunk).slice(-8192); });
    const closed = new Promise(resolve => browser.once("close", (code, signal) => {
      if (!closing || code !== 0) fail(new Error(`Chromium exited (${signal ?? code})\n${stderr}`));
      resolve();
    }));
    const deadline = setTimeout(() => {
      fail(new Error(`Chromium theme checks timed out after 30 seconds\n${stderr}`));
      browser.kill("SIGKILL");
    }, 30000);
    input.on("error", fail);
    output.on("error", fail);
    // DevTools pipe frames are NUL-delimited; only one request is in flight.
    output.setEncoding("utf8").on("data", chunk => {
      try {
        buffer += chunk;
        let end;
        while ((end = buffer.indexOf("\0")) !== -1) {
          const message = JSON.parse(buffer.slice(0, end));
          buffer = buffer.slice(end + 1);
          if (!pending || message.id !== id) continue;
          const { resolve, reject, method } = pending;
          pending = undefined;
          if (message.error) reject(new Error(`${method}: ${message.error.message}`));
          else resolve(message.result);
        }
      } catch (error) {
        fail(error);
      }
    });
    const send = (method, params, sessionId) => new Promise((resolve, reject) => {
      if (failure) return reject(failure);
      pending = { resolve, reject, method };
      input.write(JSON.stringify({ id: ++id, method, params, sessionId }) + "\0");
    });
    try {
      await checkThemes(send);
    } finally {
      closing = true;
      // Browser.close can close the pipe before acknowledging the request.
      try {
        if (failure) browser.kill();
        else await Promise.race([send("Browser.close"), closed]);
      } finally {
        await closed;
        clearTimeout(deadline);
      }
    }
    if (failure) throw failure;
  } finally {
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

run().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
