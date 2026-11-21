"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const withBrowser = require("./browser");

class Element {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.listeners = {};
    this.attributes = {};
  }
  append(...children) { this.children.push(...children); }
  prepend(...children) { this.children.unshift(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(type, callback) { this.listeners[type] = callback; }
  setAttribute(name, value) { this.attributes[name] = value; }
  querySelector(selector) {
    const stack = [...this.children].reverse();
    while (stack.length) {
      const node = stack.pop();
      if (selector === ".row-key" ? node.className === "row-key" : node.tag === "button" || node.tag === "summary") return node;
      stack.push(...[...node.children].reverse());
    }
    return null;
  }
  focus() { document.activeElement = this; }
}
const document = { createElement: tag => new Element(tag), createElementNS: (_ns, tag) => new Element(tag) };
globalThis.document = document;
async function main() {
const source = fs.readFileSync(process.argv[2] ?? "gui/dist/file-tree.js", "utf8");
const fileTree = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);
const root = new Element("div");
const entries = [
  { id: "a", key: "config/nested/same", status: "changed", supported: true },
  { id: "b", key: "config/nested/same", status: "held", group: "held" },
  { id: "c", key: "config/other", status: "changed" },
  { id: "d", key: "plain", status: "deleted" },
];
const makeLeaf = (entry, name) => {
  const li = new Element("li");
  const button = new Element("button");
  button.textContent = name;
  button.id = entry.id;
  const key = new Element("span");
  key.className = "row-key";
  button.append(key);
  li.append(button);
  return li;
};
const render = query => fileTree.render(root, entries, query, makeLeaf);
const folder = () => root.children[0].children[0].children[0];
const nested = () => folder().children[1].children[0].children[0];

assert.equal(render(""), 4);
assert.equal(folder().open, true);
assert.equal(folder().children[0].children[1].textContent, "3");
assert.equal(folder().children[0].children[0].children[0].attributes["aria-hidden"], "true");
assert.equal(nested().children[0].children[1].textContent, "2");
const duplicates = nested().children[1].children.map(li => li.children[0]);
assert.deepEqual(duplicates.map(button => button.id), ["a", "b"]);
assert.equal(duplicates[0].textContent, "same");
assert.equal(duplicates[0].children[0].children.length, 0);
assert.equal(duplicates[0].attributes["aria-label"], "config/nested/same — changed");
assert.equal(duplicates[1].attributes["aria-label"], "config/nested/same — held · details");
assert.equal(root.children[0].children[1].children[0].id, "d");

folder().open = false;
const pendingToggle = folder().listeners.toggle;
assert.equal(render("held"), 1);
pendingToggle();
assert.equal(folder().open, true);
assert.equal(folder().children[0].children[0].textContent, "config/nested");
assert.equal(folder().children[1].children[0].children[0].id, "b");
assert.equal(render(""), 4);
assert.equal(folder().open, false);
assert.equal(nested().children[1].children.length, 2);
const summary = folder().children[0];
let prevented = false;
summary.listeners.keydown({ key: "ArrowRight", preventDefault: () => { prevented = true; } });
assert.equal(folder().open, true);
assert.equal(prevented, true);
const nestedSummary = nested().children[0];
nested().open = false;
nestedSummary.listeners.keydown({ key: "ArrowLeft", preventDefault() {} });
assert.equal(document.activeElement, summary);
assert.equal(render("not found"), 0);
assert.equal(root.children.length, 0);

const deep = `${"dir/".repeat(5000)}<script>`;
assert.equal(fileTree.render(root, [{ id: "deep", key: deep, status: "changed" }], "", makeLeaf), 1);
const deepFolder = root.children[0].children[0].children[0];
assert.equal(deepFolder.children[0].children[0].textContent, "dir/".repeat(4999) + "dir");
const list = deepFolder.children[1];
assert.equal(list.children[0].children[0].textContent, "<script>");
assert.equal(list.children[0].children[0].attributes["aria-label"], `${deep} — changed · diff unavailable`);

assert.equal(fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "", makeLeaf), 1);
assert.equal(folder().children[0].children[0].textContent, ".config/helix");
assert.equal(folder().children[1].children[0].children[0].textContent, "config.toml");
folder().open = false;
folder().listeners.toggle();
fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "config", makeLeaf);
assert.equal(folder().open, true);
fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "", makeLeaf);
assert.equal(folder().open, false);

const many = Array.from({ length: 2000 }, (_, i) => ({ id: String(i), key: `common/${i}`, status: "changed" }));
assert.equal(fileTree.render(root, many, "", makeLeaf), 2000);
assert.equal(folder().open, true);
assert.equal(folder().children[1].children.length, 2000);
assert.equal(fileTree.render(root, [{ id: "fallback", key: "", status: "held" }], "held", makeLeaf), 1);
assert.equal(root.children[0].children[0].children[0].textContent, "held");

fileTree.render(root, entries, "", makeLeaf);
folder().open = false;
folder().listeners.toggle();
fileTree.render(root, many, "", makeLeaf);
fileTree.render(root, entries, "", makeLeaf);
assert.equal(folder().open, true);
await withBrowser(async send => {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const page = (method, params) => send(method, params, sessionId);
  const css = fs.readFileSync("gui/dist/style.css", "utf8") + fs.readFileSync("gui/dist/file-tree.css", "utf8");
  const html = `<style>${css}</style><div style="width: 200px"><ul class="file-tree"><li class="file-tree-folder"><details open><summary><span class="file-tree-name">.config/helix</span><span class="file-tree-count">1</span></summary><ul><li><button class="row" aria-current="true" data-group="outgoing"><span class="row-key">configuration-with-a-very-long-unbroken-name.toml</span><span class="row-status">changed-locally · system (read only)</span></button></li></ul></details></li></ul></div>`;
  const { frameTree } = await page("Page.getFrameTree");
  await page("Page.setDocumentContent", { frameId: frameTree.frame.id, html });
  for (const theme of ["light", "dark"]) {
    for (const width of [200, 320]) {
      const { result } = await page("Runtime.evaluate", { returnByValue: true, expression: `(() => {
        document.documentElement.dataset.theme = "${theme}";
        document.querySelector("div").style.width = "${width}px";
        const row = document.querySelector(".row"), key = row.querySelector(".row-key"), status = row.querySelector(".row-status");
        row.focus();
        return { height: row.offsetHeight, folderHeight: document.querySelector("summary").offsetHeight,
          fits: row.scrollWidth <= row.clientWidth && key.scrollWidth <= key.clientWidth && status.scrollWidth <= status.clientWidth,
          status: status.textContent, focused: document.activeElement === row,
          stripe: getComputedStyle(row).borderLeftWidth, connector: getComputedStyle(key, "::before").borderLeftWidth };
      })()` });
      assert.deepEqual(result.value, { height: result.value.height, folderHeight: result.value.folderHeight, fits: true,
        status: "changed-locally · system (read only)", focused: true, stripe: "0px", connector: "1px" });
      assert.ok(result.value.height >= 44 && result.value.folderHeight >= 44);
    }
  }
  await page("Emulation.setEmulatedMedia", { features: [{ name: "forced-colors", value: "active" }] });
  const { result } = await page("Runtime.evaluate", { returnByValue: true, expression: `getComputedStyle(document.querySelector(".row")).outlineStyle` });
  assert.equal(result.value, "solid");
});
console.log("file-tree checks passed");
}
main().catch(error => { console.error(error); process.exitCode = 1; });
