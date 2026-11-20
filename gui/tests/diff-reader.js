"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");

class Element {
  constructor() {
    this.children = [];
    this.attributes = {};
    this.listeners = {};
    this.className = "";
    this.hidden = false;
    this.disabled = false;
    this.scrollTop = 0;
    this.clientHeight = 100;
    this.scrollHeight = 500;
    this.offset = 0;
    this._text = "";
    this.classList = { toggle: name => {
      const classes = new Set(this.className.split(" ").filter(Boolean));
      const added = !classes.has(name);
      if (added) classes.add(name); else classes.delete(name);
      this.className = [...classes].join(" ");
      return added;
    } };
  }
  set textContent(value) { this._text = value; this.children = []; }
  get textContent() { return this._text + this.children.map(child => child.textContent).join(""); }
  append(...children) {
    for (const child of children) {
      if (child.fragment) this.children.push(...child.children);
      else this.children.push(child);
    }
  }
  replaceChildren(...children) { this._text = ""; this.children = []; this.append(...children); }
  setAttribute(name, value) { this.attributes[name] = value; }
  addEventListener(name, fn) { this.listeners[name] = fn; }
  focus() { document.activeElement = this; }
  getBoundingClientRect() { return { top: this === pre ? 0 : this.offset - pre.scrollTop }; }
  click() { if (!this.disabled) this.listeners.click(); }
}
const pre = new Element();
const tools = new Element();
const document = {
  getElementById: id => id === "diff-text" ? pre : tools,
  createElement: () => new Element(),
  createDocumentFragment: () => Object.assign(new Element(), { fragment: true }),
};
global.document = document;
const windowListeners = {};
global.window = { addEventListener: (name, listener) => { windowListeners[name] = listener; } };

async function main() {
const compiled = fs.readFileSync(process.argv[2] ?? "gui/dist/diff-reader.js");
const { show, clear } = await import(`data:text/javascript;base64,${compiled.toString("base64")}`);
const patch = "diff --git repo/x home/x\nindex 123..456 100644\n--- repo/x\n+++ home/x\n@@ -10,3 +20,4 @@ section\n same\n-old value\n+new value\n+added\n tail\n@@ -30 +41 @@\n-before\n+after\n";
show(patch);
assert.equal(tools.hidden, false);
const rows = pre.children.flatMap(child => child.className === "dr-change" ? child.children : [child])
  .filter(child => child.className.startsWith("dr-row"));
assert.equal(rows.length, 5);
assert.equal(rows[1].children[0].children[0].textContent, "11");
assert.equal(rows[1].children[1].children[0].textContent, "21");
assert.equal(rows[2].children[0].children[0].textContent, "");
assert.equal(rows[2].children[1].children[0].textContent, "22");
assert.equal(rows[1].children[0].children[1].textContent, "-");
assert.equal(rows[1].children[1].children[1].textContent, "+");
assert.ok(rows[1].children[0].children[2].children.some(child => child.className === "dr-token"));
const groups = pre.children.filter(child => child.className === "dr-change");
groups[0].offset = 40;
groups[1].offset = 200;
const [toggle, metadata, previous, next, position] = tools.children;
assert.equal(position.textContent, "Change 1 of 2");
assert.equal(previous.disabled, true);
assert.equal(next.disabled, false);
document.activeElement = next;
next.click();
assert.equal(document.activeElement, next);
assert.equal(pre.scrollTop, 200);
assert.equal(position.textContent, "Change 2 of 2");
assert.equal(next.disabled, true);
assert.equal(previous.disabled, false);
document.activeElement = toggle;
toggle.click();
assert.ok(pre.className.includes("dr-unified"));
assert.equal(toggle.attributes["aria-pressed"], "true");
assert.equal(pre.scrollTop, 200);
assert.equal(document.activeElement, toggle);
assert.equal(position.textContent, "Change 2 of 2");
groups[1].offset = 250;
windowListeners.resize();
assert.equal(pre.scrollTop, 250);
pre.scrollTop = 0;
pre.listeners.scroll();
assert.equal(position.textContent, "Change 1 of 2");
assert.equal(previous.disabled, true);
assert.equal(pre.children[0].hidden, true);
metadata.click();
assert.equal(pre.children[0].hidden, false);
assert.equal(pre.children[0].textContent, patch.split("@@")[0].trimEnd());
assert.equal(metadata.attributes["aria-expanded"], "true");
assert.equal(pre.children.filter(child => child.className === "dr-hunk").length, 2);
show(patch, "System");
assert.deepEqual(pre.children.find(child => child.className === "dr-headings").children.map(child => child.textContent), ["Repository", "System"]);
const insertion = "diff --git home/x home/x\nnew file mode 100644\nindex 000..123\n--- /dev/null\n+++ home/x\n@@ -0,0 +1 @@\n+<img onerror=alert(1)>\n";
show(insertion);
const inserted = pre.children.find(child => child.className === "dr-change").children[0];
assert.equal(inserted.children[0].children[0].textContent, "");
assert.equal(inserted.children[1].children[0].textContent, "1");
assert.equal(inserted.children[1].children[2].textContent, "<img onerror=alert(1)>");

for (const text of [
  "not a patch\n", patch.replace("+added\n", ""), patch + "extra\n",
  patch.replace("+added\n", "+added\n\\ No newline at end of file\n"),
  "diff --git repo/x home/x\nBinary files repo/x and home/x differ\n",
  patch.replace("+++ home/x", "+++ home/y"), patch.replace("old value", "\x00"),
  patch.replace("diff --git repo/x home/x", "diff --git invalid"),
]) {
  show(text);
  assert.equal(pre.textContent, text);
  assert.equal(tools.hidden, true);
}
const large = "diff --git repo/x home/x\n--- repo/x\n+++ home/x\n@@ -1,2500 +1,2500 @@\n" +
  "-old\n".repeat(2500) + "+new\n".repeat(2500);
show(large);
assert.equal(pre.textContent, large);
assert.equal(pre.children.length, 0);
clear();
assert.equal(pre.textContent, "");
assert.equal(tools.hidden, true);
console.log("diff-reader tests passed");
}

main().catch(error => { console.error(error); process.exitCode = 1; });
