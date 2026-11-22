"use strict";

const assert = require("node:assert/strict");
const withBrowser = require("./browser");

const { readCss, statusFixture, withDesktop } = require("./desktop-browser");

const css = readCss();
const html = `<!doctype html><meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<style>${css}</style><div id="tab-start" tabindex="-1"></div><button>Focus</button><select><option>Theme</option></select>`;

function prepare(theme) {
  const { dataset } = document.documentElement;
  if (theme === "system") delete dataset.theme; else dataset.theme = theme;
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
    ["ink", "add-token", "text", 4.5], ["ink", "del-token", "text", 4.5],
    ["ink", "text-selection", "text", 4.5], ["ink", "hover", "text", 4.5],
    ["on-accent", "accent-fill", "text", 4.5],
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
    for (const theme of ["system", "light", "dark"]) {
      const label = `system ${preference}, ${theme}`;
      assert.equal(await evaluate(`(${prepare})(${JSON.stringify(theme)})`), true,
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

async function checkPreferences({ page, evaluate, waitFor, resize }) {
  const scheme = value => page("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value }] });
  const theme = () => evaluate("document.querySelector('input[name=theme]:checked').value");
  const background = () => evaluate("getComputedStyle(document.body).backgroundColor");
  const ready = () => waitFor("Boolean(document.querySelector('#rows .row'))");
  await ready();
  assert.equal(await theme(), "dark");
  await evaluate("document.querySelector('input[name=theme][value=system]').click()");
  await waitFor("document.documentElement.dataset.theme === 'system'");
  await scheme("dark");
  const dark = await background();
  await scheme("light");
  assert.notEqual(await background(), dark, "System theme follows the platform");
  await evaluate(`document.querySelector("#nav-settings").click();
    document.querySelector('input[name=theme][value=dark]').click();`);
  await waitFor("document.documentElement.dataset.theme === 'dark'");
  assert.equal(await background(), dark);
  await page("Page.reload");
  await ready();
  assert.equal(await theme(), "dark", "Explicit theme survives reopening the renderer");
  assert.equal(await background(), dark);

  for (const width of [360, 960]) {
    await resize(width, 900);
    await evaluate("document.querySelector('#refresh').focus()");
    const { root } = await page("DOM.getDocument");
    const { nodeId } = await page("DOM.querySelector", { nodeId: root.nodeId, selector: "#refresh" });
    const { nodes } = await page("Accessibility.getPartialAXTree", { nodeId, fetchRelatives: false });
    assert.equal(nodes[0].role.value, "button");
    assert.equal(nodes[0].name.value, "Refresh status");
    assert.equal(await evaluate(`(() => {
      const button = document.querySelector("#refresh");
      const box = button.getBoundingClientRect();
      return box.width >= 44 && box.height >= 44 && box.right <= innerWidth &&
        document.activeElement === button && button.matches(":focus-visible");
    })()`), true, `Accessible refresh target at ${width}px`);
  }
  assert.deepEqual(await evaluate("window.violations"), []);
}

async function main() {
  await withBrowser(checkThemes);
  await withDesktop(statusFixture, checkPreferences);
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
