"use strict";

const assert = require("node:assert/strict");
const { pathToFileURL } = require("node:url");
const withBrowser = require("./browser");
const { readCss } = require("./desktop-browser");

class Element {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.listeners = {};
    this.attributes = {};
  }
  append(...children) {
    for (const child of children) child.parentElement = this;
    this.children.push(...children);
  }
  prepend(...children) { this.children.unshift(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(type, callback) { this.listeners[type] = callback; }
  setAttribute(name, value) { this.attributes[name] = value; }
  getAttribute(name) { return this.attributes[name] ?? null; }
  querySelector(selector) {
    const stack = [...this.children].reverse();
    while (stack.length) {
      const node = stack.pop();
      if (node.tag === selector) return node;
      stack.push(...[...node.children].reverse());
    }
    return null;
  }
  focus() { document.activeElement = this; }
}
const document = { createElement: tag => new Element(tag), createElementNS: (_ns, tag) => new Element(tag) };
globalThis.document = document;
async function main() {
const fileTree = await import(pathToFileURL(process.argv[2] ?? "gui/.controller-tests/file-tree.js").href);
const root = new Element("div");
const entries = [
  { id: "a", key: "config/nested/same", status: "changed", supported: true },
  { id: "b", key: "config/nested/same", status: "held", group: "held" },
  { id: "c", key: "config/other", status: "changed" },
  { id: "d", key: "plain", status: "deleted" },
];
const makeLeaf = (entry, name) => {
  const li = new Element("li");
  li.textContent = name;
  li.id = entry.id;
  const key = new Element("span");
  key.className = "row-key";
  if (entry.group === "held") {
    const details = new Element("details");
    details.open = false;
    const summary = new Element("summary");
    summary.append(key);
    details.append(summary);
    li.append(details);
  } else {
    li.append(key);
  }
  return li;
};
const render = query => fileTree.render(root, entries, query, makeLeaf);
const folder = () => root.children[0].children[0];
const nested = () => folder().children[1].children[0];
const toggle = item => item.listeners.click({ target: item });
const expanded = item => item.attributes["aria-expanded"] === "true";

assert.equal(render(""), 4);
assert.equal(expanded(folder()), true);
assert.equal(folder().children[0].children[1].textContent, "3");
assert.equal(folder().children[0].children[0].children[0].attributes["aria-hidden"], "true");
assert.equal(nested().children[0].children[1].textContent, "2");
const duplicates = nested().children[1].children;
assert.deepEqual(duplicates.map(item => item.id), ["a", "b"]);
assert.equal(duplicates[0].textContent, "same");
assert.equal(duplicates[0].children[0].children.length, 0);
assert.equal(duplicates[0].attributes["aria-label"], "config/nested/same — changed");
assert.equal(duplicates[1].attributes["aria-label"], "config/nested/same — held · details");
const held = duplicates[1].children[0];
assert.equal(expanded(duplicates[1]), false);
assert.equal(held.children[0].attributes.tabindex, "-1");
held.open = true;
held.listeners.toggle();
assert.equal(expanded(duplicates[1]), true);
assert.equal(root.children[0].children[1].id, "d");

toggle(folder());
assert.equal(render("held"), 1);
assert.equal(expanded(folder()), true);
assert.equal(folder().children[0].children[0].textContent, "config/nested");
assert.equal(folder().children[1].children[0].id, "b");
assert.equal(render(""), 4);
assert.equal(expanded(folder()), false);
assert.equal(nested().children[1].children.length, 2);
toggle(nested());
fileTree.reveal(nested().children[1].children[0]);
assert.equal(expanded(folder()), true);
assert.equal(expanded(nested()), true);
assert.equal(render("not found"), 0);
assert.equal(root.children.length, 0);

const deep = `${"dir/".repeat(5000)}<script>`;
assert.equal(fileTree.render(root, [{ id: "deep", key: deep, status: "changed" }], "", makeLeaf), 1);
const deepFolder = root.children[0].children[0];
assert.equal(deepFolder.children[0].children[0].textContent, "dir/".repeat(4999) + "dir");
const list = deepFolder.children[1];
assert.equal(list.children[0].textContent, "<script>");
assert.equal(list.children[0].attributes["aria-label"], `${deep} — changed · diff unavailable`);

assert.equal(fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "", makeLeaf), 1);
assert.equal(folder().children[0].children[0].textContent, ".config/helix");
assert.equal(folder().children[1].children[0].textContent, "config.toml");
toggle(folder());
fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "config", makeLeaf);
assert.equal(expanded(folder()), true);
fileTree.render(root, [{ id: "compact", key: ".config/helix/config.toml", status: "changed", supported: true }], "", makeLeaf);
assert.equal(expanded(folder()), false);

const many = Array.from({ length: 2000 }, (_, i) => ({ id: String(i), key: `common/${i}`, status: "changed" }));
assert.equal(fileTree.render(root, many, "", makeLeaf), 2000);
assert.equal(expanded(folder()), true);
assert.equal(folder().children[1].children.length, 2000);
assert.equal(fileTree.render(root, [{ id: "fallback", key: "", status: "held" }], "held", makeLeaf), 1);
assert.equal(root.children[0].children[0].textContent, "held");

fileTree.render(root, entries, "", makeLeaf);
toggle(folder());
fileTree.render(root, many, "", makeLeaf);
fileTree.render(root, entries, "", makeLeaf);
assert.equal(expanded(folder()), true);
await withBrowser(async send => {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const page = (method, params) => send(method, params, sessionId);
  const css = readCss();
  const html = `<style>${css}</style><div style="width: 200px"><ul class="file-tree" role="tree"><li class="file-tree-folder" role="treeitem" aria-expanded="true"><div class="file-tree-branch"><span class="file-tree-name">.config/helix</span><span class="file-tree-count">1</span></div><ul role="group"><li class="row" role="treeitem" tabindex="0" aria-current="true" data-group="outgoing"><span class="row-key">configuration-with-a-very-long-unbroken-name.toml</span><span class="row-status">changed-locally · system (read only)</span></li></ul></li></ul></div>`;
  const { frameTree } = await page("Page.getFrameTree");
  await page("Page.setDocumentContent", { frameId: frameTree.frame.id, html });
  for (const theme of ["light", "dark"]) {
    for (const width of [200, 320]) {
      const { result } = await page("Runtime.evaluate", { returnByValue: true, expression: `(() => {
        document.documentElement.dataset.theme = "${theme}";
        document.querySelector("div").style.width = "${width}px";
        const row = document.querySelector(".row"), key = row.querySelector(".row-key"), status = row.querySelector(".row-status");
        row.focus();
        return { height: row.offsetHeight, folderHeight: document.querySelector(".file-tree-branch").offsetHeight,
          fits: row.scrollWidth <= row.clientWidth && key.scrollWidth <= key.clientWidth && status.scrollWidth <= status.clientWidth,
          status: status.textContent, focused: document.activeElement === row,
          connector: getComputedStyle(key, "::before").borderLeftWidth };
      })()` });
      assert.deepEqual(result.value, { height: result.value.height, folderHeight: result.value.folderHeight, fits: true,
        status: "changed-locally · system (read only)", focused: true, connector: "1px" });
      assert.ok(result.value.height >= 36 && result.value.folderHeight >= 36);
    }
  }
  await page("Emulation.setEmulatedMedia", { features: [{ name: "forced-colors", value: "active" }] });
  const { result } = await page("Runtime.evaluate", { returnByValue: true, expression: `getComputedStyle(document.querySelector(".row")).outlineStyle` });
  assert.equal(result.value, "solid");
});
console.log("file-tree checks passed");
}
main().catch(error => { console.error(error); process.exitCode = 1; });
