"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const withBrowser = require("./browser");

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

  const appHtml = fs.readFileSync(path.join(__dirname, "../dist/index.html"), "utf8")
    .replace(/<link rel="stylesheet"[^>]*>/g, "")
    .replace(/<script type="module"[^>]*><\/script>/, `<style>${css}</style>`);
  await page("Page.setDocumentContent", { frameId: frameTree.frame.id, html: appHtml });
  await page("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "dark" }] });
  assert.deepEqual(await evaluate(`({
    theme: document.documentElement.dataset.theme,
    selection: document.querySelector("#theme-mode").value,
    scheme: getComputedStyle(document.documentElement).colorScheme
  })`), { theme: "light", selection: "light", scheme: "light" });
  assert.equal(await evaluate(`document.documentElement.dataset.theme = "system"; getComputedStyle(document.documentElement).colorScheme`), "light dark");
  assert.equal(await evaluate(`getComputedStyle(document.body).backgroundColor`),
    await evaluate(`document.documentElement.dataset.theme = "dark"; getComputedStyle(document.body).backgroundColor`));

  await evaluate(`document.querySelector("#cancel").hidden = false;
    document.querySelector("#review").hidden = false; document.querySelector("#diff").hidden = false;`);
  const { root } = await page("DOM.getDocument");
  for (const [id, name] of [["refresh", "Refresh status"], ["cancel", "Cancel read"],
    ["close-review", "Close review"], ["close-diff", "Close diff"]]) {
    const { nodeId } = await page("DOM.querySelector", { nodeId: root.nodeId, selector: `#${id}` });
    const { nodes } = await page("Accessibility.getPartialAXTree", { nodeId, fetchRelatives: false });
    assert.equal(nodes[0].role.value, "button");
    assert.equal(nodes[0].name.value, name);
    if (id === "cancel") {
      assert.ok(nodes[0].properties.some(property => property.name === "disabled" && property.value.value));
      await evaluate(`document.querySelector("#cancel").disabled = false`);
    }
    for (const width of [360, 960]) {
      await page("Emulation.setDeviceMetricsOverride", { width, height: 900, deviceScaleFactor: 1, mobile: false });
      const report = await evaluate(`(() => {
        const button = document.querySelector("#${id}");
        button.focus();
        const rect = button.getBoundingClientRect(), tip = getComputedStyle(button, "::after");
        return { width: rect.width, height: rect.height, radius: getComputedStyle(button).borderRadius,
          focused: button.matches(":focus-visible"), tip: tip.visibility, text: tip.content,
          tipLeft: rect.right - parseFloat(tip.width), tipRight: rect.right };
      })()`);
      assert.ok(report.width >= 44 && report.height >= 44, `${id}: target size`);
      assert.equal(report.radius, "50%");
      assert.equal(report.focused, true, `${id}: keyboard focus`);
      assert.equal(report.tip, "visible");
      assert.equal(report.text, `"${name}"`);
      assert.ok(report.tipLeft >= 0 && report.tipRight <= width, `${id}: tooltip fits at ${width}px`);
      const center = await evaluate(`(() => {
        const rect = document.querySelector("#${id}").getBoundingClientRect();
        document.activeElement.blur();
        return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
      })()`);
      await page("Input.dispatchMouseEvent", { type: "mouseMoved", ...center });
      assert.equal(await evaluate(`getComputedStyle(document.querySelector("#${id}"), "::after").visibility`), "visible",
        `${id}: hover tooltip`);
      await page("Input.dispatchMouseEvent", { type: "mouseMoved", x: 0, y: 0 });
    }
  }

  const readerCss = fs.readFileSync(path.join(__dirname, "../dist/diff-reader.css"), "utf8");
  await page("Page.setDocumentContent", { frameId: frameTree.frame.id, html: `<!doctype html>
    <style>${css}\n${readerCss}</style><main><div class="desk">
    <section id="entries">File browser</section><section id="diff"><div id="reader-tools"><button class="dr-toggle">Unified view</button></div>
    <pre id="diff-text" class="diff-reader"><span class="dr-row"><span class="dr-cell">old</span><span class="dr-cell">new</span></span></pre>
    </section></div></main>` });
  for (const [width, height, navigator, columns] of [
    [1440, 900, true, 2], [960, 640, true, 1], [360, 640, false, 1],
    [2560, 1080, true, 2], [3440, 1440, true, 2], [5120, 1440, true, 2], [960, 640, true, 1]
  ]) {
    await page("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
    const layout = await evaluate(`({
      navigator: document.querySelector("#entries").getClientRects().length > 0,
      columns: getComputedStyle(document.querySelector(".dr-row")).gridTemplateColumns.split(" ").length,
      toggle: document.querySelector(".dr-toggle").getClientRects().length > 0,
      bodySize: parseFloat(getComputedStyle(document.body).fontSize),
      fits: document.documentElement.scrollWidth <= innerWidth
    })`);
    assert.deepEqual(layout, { navigator, columns, toggle: columns === 2, bodySize: 18, fits: true }, `resized to ${width}px`);
    if (width >= 2560) {
      const sizes = await evaluate(`[document.querySelector("#entries").clientWidth, document.querySelector("#diff").clientWidth]`);
      assert.ok(sizes[0] <= 25 * layout.bodySize && sizes[1] > width * .75, `ultrawide ${width}px: panes ${sizes}`);
    }
  }
}

withBrowser(checkThemes).catch(error => {
  console.error(error);
  process.exitCode = 1;
});
