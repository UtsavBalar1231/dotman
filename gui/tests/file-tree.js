"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");

class Element {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.listeners = {};
    this.attributes = {};
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(type, callback) { this.listeners[type] = callback; }
  setAttribute(name, value) { this.attributes[name] = value; }
  querySelector() {
    const stack = [...this.children].reverse();
    while (stack.length) {
      const node = stack.pop();
      if (node.tag === "button" || node.tag === "summary") return node;
      stack.push(...[...node.children].reverse());
    }
    return null;
  }
  focus() { document.activeElement = this; }
}
const document = { createElement: tag => new Element(tag) };
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
  li.append(button);
  return li;
};
const render = query => fileTree.render(root, entries, query, makeLeaf);
const folder = () => root.children[0].children[0].children[0];
const nested = () => folder().children[1].children[0].children[0];

assert.equal(render(""), 4);
assert.equal(folder().open, true);
assert.equal(folder().children[0].children[1].textContent, "3");
assert.equal(nested().children[0].children[1].textContent, "2");
const duplicates = nested().children[1].children.map(li => li.children[0]);
assert.deepEqual(duplicates.map(button => button.id), ["a", "b"]);
assert.equal(duplicates[0].textContent, "same");
assert.equal(duplicates[0].attributes["aria-label"], "config/nested/same — changed");
assert.equal(duplicates[1].attributes["aria-label"], "config/nested/same — held · details");
assert.equal(root.children[0].children[1].children[0].id, "d");

folder().open = false;
const pendingToggle = folder().listeners.toggle;
assert.equal(render("held"), 1);
pendingToggle();
assert.equal(folder().open, true);
assert.equal(nested().children[1].children[0].children[0].id, "b");
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
let list = root.children[0];
for (let i = 0; i < 5000; i++) list = list.children[0].children[0].children[1];
assert.equal(list.children[0].children[0].textContent, "<script>");
assert.equal(list.children[0].children[0].attributes["aria-label"], `${deep} — changed · diff unavailable`);

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
console.log("file-tree checks passed");
}
main().catch(error => { console.error(error); process.exitCode = 1; });
