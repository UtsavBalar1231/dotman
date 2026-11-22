"use strict";

const assert = require("node:assert/strict");

class Element {
  constructor(id = "", tag = "") {
    this.id = id;
    this.tag = tag;
    this.hidden = false;
    this.disabled = false;
    this.value = "";
    this.textContent = "";
    this.children = [];
    this.listeners = {};
    this.attributes = {};
    this.isConnected = true;
  }
  append(...children) { this.children.push(...children); }
  prepend(...children) { this.children.unshift(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(type, fn) { this.listeners[type] = fn; }
  setAttribute(name, value) { this.attributes[name] = value; }
  getAttribute(name) { return this.attributes[name] ?? null; }
  removeAttribute(name) { delete this.attributes[name]; }
  querySelector(selector) {
    const tags = selector.split(",").map(tag => tag.trim());
    const visit = node => {
      for (const child of node.children) {
        if (tags.includes(child.tag)) return child;
        const found = visit(child);
        if (found) return found;
      }
      return null;
    };
    return visit(this);
  }
  focus() { document.activeElement = this; }
  scrollIntoView(options) { this.scrolled = options; }
  click() { return this.listeners.click(); }
}

const ids = ["refresh", "cancel", "diff", "comparison-body", "diff-key", "diff-message", "diff-text",
  "rows", "search", "entries", "context", "paths", "message", "notice",
  "cancel-review", "remote", "clean", "held-counts", "close-diff", "review", "review-items", "review-message",
  "review-remote", "close-review", "sync-reviewed", "sync-mass-delete", "sync-lines", "leave-lines", "line-summary", "selected-effect", "selected-lines", "line-edits", "line-edit-list",
  "commit-editor", "commit-message", "commit-error",
  "recovery", "recovery-message", "recovery-steps", "decisions", "decision-message",
  "decision-items", "decision-results", "clear-decisions", "apply-decisions", "diff-actions", "diff-selection",
  "setup", "machine-files", "machine-file-details", "review-identity", "identity-result", "identity-message", "review-hooks", "hooks-result", "hooks-message",
  "age-controls", "create-age", "import-age", "age-message",
  "start-fresh", "start-import", "setup-result", "setup-message", "setup-retry", "clone-controls", "clone-source", "start-clone",
  "repository-path", "diff-context", "group-folders", "group-status",
  "guide-title", "guide-description", "attention-list", "next-undecided", "reader-tools"];
ids.push("policy-controls", "policy-directories", "policy-ignores", "policy-message");
ids.push("variant-result", "variant-message");
ids.push("replacement-review", "replacement-title", "replacement-source", "replacement-target", "replacement-back", "replacement-batch", "decisions-title");
ids.push("nav-files", "nav-review", "nav-settings", "settings", "page-heading", "paths-unavailable", "age-unavailable");
ids.push("recovery-check", "recovery-paths", "recovery-finding", "recovery-details", "setup-check", "setup-paths", "setup-finding", "setup-guidance");
ids.push("review-decisions", "close-decisions", "retry-decisions");
ids.push("review-body", "source-edit-details", "review-title", "review-icon", "review-return", "review-footnote");
const elements = Object.fromEntries(ids.map(id => [id, new Element(id)]));
elements["variant-result"].hidden = true;
elements["variant-message"].setAttribute("role", "status");
elements["variant-message"].setAttribute("aria-live", "polite");
elements["variant-result"].append(elements["variant-message"]);
elements["group-folders"].setAttribute("aria-pressed", "false");
elements["group-status"].setAttribute("aria-pressed", "true");
const document = {
  activeElement: null,
  documentElement: new Element(),
  body: new Element(),
  getElementById: id => elements[id],
  createElement: tag => new Element("", tag),
  createElementNS: (_namespace, tag) => new Element("", tag),
  createDocumentFragment: () => new Element(),
};
let resolveDiff;
let rejectDiff;
const diffRequests = [];
let resolveReview;
let resolveWriter;
let rejectWriter;
let resolveCancel;
let delayCancel = false;
let refreshes = 0;
let statusResponse;
let delayRefresh = false;
let resolveRefresh;
let rejectRefresh;
const calls = [];
const copies = [];
let copyFails = false;
const writes = [];
const fileWrites = [];
const identityWrites = [];
const hookWrites = [];
const keyWrites = [];
const setupWrites = [];
const cloneWrites = [];
const importWrites = [];
const policyWrites = [];
const variantWrites = [];
const heldWrites = [];
const discards = [];
let rejectDiscard;
const status = {
  success: true, message: "Status loaded", key_stripped: false, clean: 0, remote: "",
  context: { home: "/h", repo: "/r", config: "/c", state: "/s", identity: "/i", machine: "m" },
  entries: [{ id: "1:0", key: "x", status: "changed-locally", group: "outgoing", supported: true, system: false }],
};
const invoke = (command, args) => {
  calls.push(command);
  if (command === "refresh") {
    refreshes++;
    if (delayRefresh) return new Promise((resolve, reject) => { resolveRefresh = resolve; rejectRefresh = reject; });
    return Promise.resolve(statusResponse ?? status);
  }
  if (command === "cancel") {
    if (delayCancel) return new Promise(resolve => { resolveCancel = resolve; });
    return Promise.resolve();
  }
  if (command === "open_diff") {
    diffRequests.push(args);
    return new Promise((resolve, reject) => { resolveDiff = resolve; rejectDiff = reject; });
  }
  if (command === "open_review") return new Promise(resolve => { resolveReview = resolve; });
  if (command === "run_reviewed_sync") {
    assert.ok(args.progress instanceof Channel);
    assert.equal(typeof args.progress.onmessage, "function");
    writes.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "apply_reviewed_files") {
    fileWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "accept_reviewed_identity") {
    identityWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "approve_pending_hooks") {
    hookWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_key") {
    keyWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_held") {
    heldWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_host_variant") {
    variantWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "review_policy") {
    policyWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_fresh_setup") {
    setupWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_clone_setup") {
    cloneWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "run_import_setup") {
    importWrites.push(args);
    return new Promise((resolve, reject) => { resolveWriter = resolve; rejectWriter = reject; });
  }
  if (command === "discard_review") {
    discards.push(args.id);
    return new Promise((resolve, reject) => { rejectDiscard = reject; });
  }
  throw new Error(command);
};
class Channel {}
const window = { __TAURI__: { core: { invoke, Channel } }, scrollX: 0, scrollY: 1234, addEventListener() {},
  navigator: { clipboard: { writeText: text => {
    copies.push(text);
    return copyFails ? Promise.reject(new Error("denied")) : Promise.resolve();
  } } },
  scrollTo: (...args) => { window.restored = args; } };
globalThis.document = document;
globalThis.window = window;
globalThis.HTMLDetailsElement = class extends Element {};

async function tick() { await new Promise(resolve => setImmediate(resolve)); }

async function main() {
  const { getWorkspaceView } = await import("../.controller-tests/workspace-view.js");
  await import("../.controller-tests/app.js");
  await tick();
  elements["group-status"].click();
  assert.equal(elements["guide-title"].textContent, "File comparison");
  assert.equal(elements["attention-list"].children.length, 0);
  elements["nav-settings"].click();
  assert.equal(getWorkspaceView().page, "settings");
  assert.equal(document.activeElement, elements["page-heading"]);
  assert.equal(elements.settings.hidden, false);
  elements["nav-files"].click();
  elements["nav-review"].click();
  resolveReview({ id: "navigation-review", outcome: "review", message: "Preview", items: [] });
  await tick();
  elements["nav-settings"].click();
  assert.deepEqual(discards, ["navigation-review"]);
  assert.equal(elements.review.hidden, true);
  assert.equal(getWorkspaceView().page, "settings");
  assert.equal(document.activeElement, elements["page-heading"]);
  assert.equal(writes.length, 0);
  discards.length = 0;
  elements["nav-files"].click();
  statusResponse = { ...status, hook_review_id: "hooks:7" };
  elements.refresh.click();
  await tick();
  assert.equal(elements["review-hooks"].hidden, false);
  elements["review-hooks"].click();
  elements["review-hooks"].click();
  assert.deepEqual(hookWrites, [{ id: "hooks:7" }]);
  assert.equal(document.activeElement, elements["hooks-message"]);
  resolveWriter({ outcome: "declined", message: "No hooks ran.", refresh: false });
  await tick();
  assert.equal(elements["hooks-message"].textContent, "declined: No hooks ran.");
  assert.equal(elements["review-hooks"].hidden, true);
  statusResponse = { ...status, key_review_id: "key:7" };
  elements.refresh.click();
  await tick();
  elements["nav-settings"].click();
  assert.equal(elements["age-controls"].hidden, false);
  elements["import-age"].click();
  assert.equal(getWorkspaceView().busy, true);
  elements["nav-files"].click();
  assert.equal(getWorkspaceView().page, "settings");
  elements["import-age"].click();
  assert.deepEqual(keyWrites, [{ id: "key:7", action: "import" }]);
  resolveWriter({ outcome: "declined", message: "Identity installation was not approved.", refresh: true });
  await tick();
  assert.match(elements["identity-message"].textContent, /not approved/);
  statusResponse = { ...status, policy_review_id: "policy:4" };
  elements.refresh.click();
  await tick();
  elements["policy-ignores"].click();
  elements["policy-ignores"].click();
  assert.deepEqual(policyWrites, [{ id: "policy:4", action: "ignores" }]);
  resolveWriter({ outcome: "declined", message: "No changes.", refresh: true });
  await tick();
  assert.equal(elements["policy-message"].textContent, "declined: No changes.");
  elements["nav-files"].click();
  statusResponse = null;
  elements.refresh.click();
  await tick();
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements.decisions.hidden, true);
  statusResponse = { ...status, entries: [{ ...status.entries[0], key: "dir/<file>", status: "changed-in-repo", group: "incoming" }] };
  elements.refresh.click();
  await tick();
  elements.rows.children[0].children[1].children[0].children[0].click();
  assert.equal(elements["diff-key"].textContent, "dir/<file>");
  assert.match(elements["diff-context"].textContent, /repository copy has changed/);
  resolveDiff({ outcome: "ready", message: "different", text: "", file_actions: [] });
  await tick();
  assert.match(elements["diff-message"].textContent, /Replacement review is not available/);
  assert.equal(elements["diff-actions"].children.length, 0);
  assert.equal(fileWrites.length, 0);
  elements["close-diff"].click();
  statusResponse = null;
  elements.refresh.click();
  await tick();
  statusResponse = { ...status, entries: [
    { id: "conflict", key: "other", status: "both-changed", group: "decision", supported: true, system: false },
    { id: "incoming", key: "config/<file>", status: "changed-in-repo", group: "incoming", supported: true, system: false,
      machine_file: { selected: "config/<file>{host=test}", recorded: null, alternatives: [], kind: "plain", identity_changed: false, ambiguous: false } },
  ] };
  elements.refresh.click();
  await tick();
  const allNodes = node => [node, ...node.children.flatMap(allNodes)];
  const findRow = key => allNodes(elements.rows).find(node => node.tag === "button" && node.title === key);
  findRow("other").click();
  resolveDiff({ success: true, outcome: "ready", message: "different", text: "", file_actions: ["keep_local"] });
  await tick();
  elements["diff-actions"].children[0].click();
  findRow("config/<file>").click();
  resolveDiff({ success: true, outcome: "ready", message: "different", text: "", file_actions: ["take_repo"] });
  await tick();
  const replacement = elements["diff-actions"].children[0];
  assert.equal(replacement.textContent, "Review replacement");
  assert.equal(replacement.className, "review-action");
  assert.equal(replacement.children[0].attributes["aria-hidden"], "true");
  assert.equal(elements["diff-actions"].children.length, 1);
  replacement.click();
  assert.equal(elements["replacement-review"].hidden, false);
  assert.equal(document.activeElement, elements["replacement-title"]);
  assert.equal(elements["replacement-source"].textContent, "/r/config/<file>{host=test}");
  assert.equal(elements["replacement-target"].textContent, "/h/config/<file>");
  assert.equal(elements["decision-items"].children.length, 1);
  assert.equal(fileWrites.length, 0);
  elements["replacement-back"].click();
  assert.equal(document.activeElement, replacement);
  assert.equal(elements["replacement-review"].hidden, true);
  assert.equal(elements["decision-items"].children.length, 1);
  replacement.click();
  elements["close-diff"].click();
  elements["replacement-batch"].click();
  assert.equal(elements["decision-items"].children.length, 1);
  assert.equal(fileWrites.length, 0);
  findRow("config/<file>").click();
  resolveDiff({ success: true, outcome: "ready", message: "different", text: "", file_actions: ["take_repo"] });
  await tick();
  elements["diff-actions"].children[0].click();
  assert.equal(elements["decision-items"].children.length, 1);
  elements["replacement-batch"].click();
  assert.equal(elements["replacement-review"].hidden, true);
  assert.equal(document.activeElement, elements["diff-actions"].children[0]);
  assert.equal(elements.decisions.hidden, true);
  assert.notEqual(getWorkspaceView().queued, null);
  elements["review-decisions"].click();
  assert.equal(document.activeElement, elements["decisions-title"]);
  assert.equal(elements["decision-items"].children.length, 2);
  assert.equal(fileWrites.length, 0);
  elements["diff-actions"].children[0].click();
  elements["replacement-back"].click();
  assert.equal(elements["decision-items"].children.length, 2);
  elements["apply-decisions"].click();
  assert.deepEqual(fileWrites.at(-1), { answers: [{ id: "conflict", action: "keep_local" }, { id: "incoming", action: "take_repo" }] });
  resolveWriter({ outcome: "declined", message: "No changes.", items: [], refresh: false });
  await tick();
  assert.equal(elements["retry-decisions"].hidden, false);
  replacement.click();
  assert.equal(elements["decision-items"].children.length, 0);
  assert.equal(fileWrites.length, 1);
  elements["decision-message"].textContent = "";
  fileWrites.length = 0;
  for (const scenario of [
    { entry: { system: true }, response: { success: true, outcome: "ready", file_actions: ["take_repo"] } },
    { entry: { supported: false }, response: null },
    { entry: {}, response: { success: false, outcome: "ready", file_actions: ["take_repo"] } },
    { entry: {}, response: { success: true, outcome: "error", file_actions: ["take_repo"] } },
    { entry: {}, response: { success: true, outcome: "ready", file_actions: ["keep_local"] } },
    { entry: { key: "bad\u202ename" }, response: { success: true, outcome: "ready", file_actions: ["take_repo"] } },
    { entry: { machine_file: { selected: "bad\nsource" } }, response: { success: true, outcome: "ready", file_actions: ["take_repo"] } },
  ]) {
    statusResponse = { ...status, entries: [{ ...status.entries[0], id: "negative", status: "changed-in-repo", group: "incoming", ...scenario.entry }] };
    elements.refresh.click();
    await tick();
    findRow(scenario.entry.key ?? "x").click();
    if (scenario.response) resolveDiff({ message: "", text: "", ...scenario.response });
    await tick();
    assert.equal(elements["diff-actions"].children.length, 0);
    assert.equal(elements["decision-items"].children.length, 0);
    assert.equal(fileWrites.length, 0);
    elements["close-diff"].click();
  }
  statusResponse = { ...status, entries: [{ ...status.entries[0], id: "cancel-incoming", status: "changed-in-repo", group: "incoming" }] };
  elements.refresh.click();
  await tick();
  findRow("x").click();
  elements.cancel.click();
  resolveDiff({ success: true, outcome: "ready", message: "late", text: "", file_actions: ["take_repo"] });
  await tick();
  assert.equal(elements["diff-actions"].children.length, 0);
  assert.equal(elements["decision-items"].children.length, 0);
  assert.equal(fileWrites.length, 0);
  statusResponse = null;
  elements.refresh.click();
  await tick();
  const edits = [
    { id: "opaque-1", old_line: 1, new_line: 1, before: "old", after: "new" },
    { id: "opaque-2", old_line: null, new_line: 2, before: null, after: "added" },
    { id: "opaque-3", old_line: 3, new_line: null, before: "removed", after: null },
  ];
  elements["nav-review"].click();
  assert.equal(getWorkspaceView().page, "review");
  assert.equal(getWorkspaceView().busy, true);
  elements["nav-settings"].click();
  assert.equal(getWorkspaceView().page, "review");
  resolveReview({ id: "line-review", outcome: "review", message: "loaded", commit_message: "message",
    items: [{ group: "Capture", text: "full/path", diff_id: "capture" }] });
  await tick();
  const capture = elements["review-items"].children[0].children[1].children[0].children[0];
  capture.click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", selected_push: false, line_edits: edits });
  await tick();
  const boxes = elements["line-edit-list"].children.map(label => label.children[0]);
  boxes[0].checked = true; boxes[0].listeners.change();
  assert.equal(elements["commit-message"].value, "sync: selected edits");
  assert.match(elements["selected-effect"].textContent, /No remote/);
  assert.equal(elements["sync-lines"].textContent, "Commit selected lines");
  boxes[1].checked = true; boxes[1].listeners.change();
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements["selected-lines"].children.length, 2);
  assert.match(elements["selected-lines"].children[0].textContent, /full\/path/);
  elements["close-diff"].click();
  assert.equal(getWorkspaceView().page, "review");
  assert.equal(elements.review.hidden, false);
  assert.equal(document.activeElement, capture);
  boxes[2].checked = true; boxes[2].listeners.change();
  assert.equal(elements["selected-lines"].children.length, 2);
  capture.click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", line_edits: edits });
  await tick();
  const reopened = elements["line-edit-list"].children.map(label => label.children[0]);
  assert.equal(reopened[0].checked, true);
  assert.equal(reopened[1].checked, true);
  elements["close-diff"].click();
  capture.click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", line_edits: [{ ...edits[0], id: "changed" }, edits[1], edits[2]] });
  await tick();
  assert.equal(elements["line-edit-list"].children[0].children[0].checked, false);
  assert.equal(elements["selected-lines"].children.length, 0);
  assert.equal(elements["sync-reviewed"].hidden, true);
  const changed = elements["line-edit-list"].children[0].children[0];
  changed.checked = true; changed.listeners.change();
  elements["close-diff"].click();
  capture.click();
  rejectDiff(new Error("read failed"));
  await tick();
  assert.equal(elements["selected-lines"].children.length, 0);
  assert.equal(elements["line-edits"].hidden, true);
  elements["close-diff"].click();
  capture.click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", line_edits: edits });
  await tick();
  const cleared = elements["line-edit-list"].children.map(label => label.children[0]);
  cleared[0].checked = true; cleared[0].listeners.change();
  cleared[1].checked = true; cleared[1].listeners.change();
  cleared[0].checked = false; cleared[0].listeners.change();
  cleared[1].checked = false; cleared[1].listeners.change();
  elements["close-diff"].click();
  assert.equal(elements["sync-lines"].disabled, true);
  elements["sync-reviewed"].click();
  elements["sync-lines"].click();
  assert.equal(writes.length, 0);
  elements["leave-lines"].click();
  assert.equal(elements["commit-message"].value, "message");
  assert.equal(elements["sync-reviewed"].hidden, false);
  assert.equal(elements["sync-lines"].hidden, true);
  elements["commit-message"].value = "chosen by user";
  capture.click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", selected_push: true, line_edits: edits });
  await tick();
  const selected = elements["line-edit-list"].children.map(label => label.children[0]);
  selected[0].checked = true; selected[0].listeners.change();
  selected[2].checked = true; selected[2].listeners.change();
  assert.equal(elements["sync-lines"].textContent, "Commit selected lines and push");
  elements["close-diff"].click();
  elements["sync-lines"].click();
  assert.deepEqual(writes.at(-1).selectedLines, ["opaque-1", "opaque-3"]);
  assert.equal(writes.at(-1).commitMessage, "chosen by user");
  resolveWriter({ outcome: "declined", message: "declined", items: [], refresh: false });
  await tick();
  assert.equal(writes.length, 1);
  writes.length = 0;
  elements.refresh.click();
  await tick();
  for (const outcome of ["stale", "push_failed"]) {
    elements["nav-review"].click();
    resolveReview({ id: `review-${outcome}`, outcome: "review", message: "", commit_message: "message",
      items: [{ group: "Capture", text: "full/path", diff_id: "capture" }] });
    await tick();
    elements["review-items"].children[0].children[1].children[0].children[0].click();
    resolveDiff({ success: true, outcome: "ready", message: "", text: "", line_edits: edits });
    await tick();
    const box = elements["line-edit-list"].children[0].children[0];
    assert.equal(box.checked, false);
    box.checked = true; box.listeners.change();
    elements["close-diff"].click();
    elements["sync-lines"].click();
    resolveWriter({ outcome, message: outcome, items: [], refresh: false });
    await tick();
    assert.equal(writes.length, 1);
    writes.length = 0;
    elements.refresh.click();
    await tick();
  }
  elements["nav-review"].click();
  resolveReview({ id: "malformed", outcome: "review", message: "", commit_message: "message",
    items: [{ group: "Capture", text: "full/path", diff_id: "capture" }] });
  await tick();
  elements["review-items"].children[0].children[1].children[0].children[0].click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "", line_edits: [edits[0], edits[0]] });
  await tick();
  assert.equal(elements["line-edits"].hidden, true);
  elements["close-diff"].click();
  elements["close-review"].click();
  discards.length = 0;
  assert.equal(elements["group-folders"].attributes["aria-pressed"], "false");
  assert.equal(elements["group-status"].attributes["aria-pressed"], "true");
  elements["group-folders"].click();
  statusResponse = { ...status, entries: [{ ...status.entries[0], key: "config/x", status: "template-drift", editable: true }] };
  elements.refresh.click();
  await tick();
  const folder = elements.rows.children[0].children[0];
  assert.equal(folder.attributes.role, "treeitem");
  assert.equal(folder.children[0].children[0].textContent, "config");
  assert.equal(folder.attributes["aria-expanded"], "true");
  folder.listeners.click({ target: folder });
  assert.equal(folder.children[1].hidden, true);
  folder.listeners.click({ target: folder });
  assert.equal(folder.attributes["aria-expanded"], "true");
  folder.children[1].children[0].click();
  assert.equal(diffRequests.at(-1).id, "1:0");
  const details = elements["source-edit-details"].children[0];
  assert.equal(details.tag, "button");
  assert.equal(details.textContent, "Edit source");
  resolveDiff({ outcome: "ready", message: "folder", text: "" });
  await tick();
  elements["close-diff"].click();
  elements["group-status"].click();
  assert.equal(elements["group-folders"].attributes["aria-pressed"], "false");
  assert.equal(elements["group-status"].attributes["aria-pressed"], "true");
  statusResponse = null;
  elements.refresh.click();
  await tick();
  const row = elements.rows.children[0].children[1].children[0].children[0];
  elements.rows.scrollTop = 37;
  row.click();
  assert.equal(elements["diff-actions"].children.length, 0);
  assert.equal(elements.entries.hidden, false);
  assert.equal(elements.diff.hidden, false);
  assert.equal(elements["diff-context"].textContent, "Local change · Repository vs home");
  assert.equal(document.activeElement, elements["close-diff"]);
  resolveDiff({ outcome: "ready", message: "different", text: "old\nnew\n" });
  await tick();
  assert.equal(elements["diff-text"].textContent, "old\nnew\n");
  elements["close-diff"].click();
  assert.equal(elements["diff-text"].textContent, "");
  assert.equal(elements.entries.hidden, false);
  assert.equal(document.activeElement, row);
  assert.equal(elements.rows.scrollTop, 37);
  assert.equal(row.scrolled.block, "nearest");
  assert.deepEqual(window.restored, [0, 1234]);
  row.click();
  resolveDiff({ outcome: "ready", message: "different", text: "" });
  await tick();
  elements.search.value = "x";
  elements.search.listeners.input();
  const replacedRow = elements.rows.children[0].children[1].children[0].children[0];
  elements["close-diff"].click();
  assert.equal(document.activeElement, replacedRow);
  replacedRow.click();
  resolveDiff({ outcome: "ready", message: "different", text: "" });
  await tick();
  elements.search.value = "not found";
  elements.search.listeners.input();
  elements["close-diff"].click();
  assert.equal(document.activeElement, elements.search);
  elements.search.value = "";
  elements.search.listeners.input();
  elements["nav-review"].click();
  assert.equal(elements.entries.hidden, false);
  assert.equal(document.activeElement, elements["close-review"]);
  resolveReview({ id: "7:1", outcome: "review", message: "loaded", items: [{ group: "Hooks due", text: "<script>alert(1)</script>" }] });
  await tick();
  assert.equal(elements["review-items"].children[0].children[1].children[0].textContent, "<script>alert(1)</script>");
  assert.equal(elements["sync-reviewed"].hidden, false);
  elements["close-review"].click();
  assert.equal(document.activeElement, elements["nav-review"]);
  assert.equal(elements.entries.hidden, false);
  assert.deepEqual(window.restored, [0, 1234]);
  assert.deepEqual(discards, ["7:1"]);
  elements["nav-review"].click();
  resolveReview({ id: "7:2", outcome: "review", message: "new", items: [] });
  await tick();
  rejectDiscard(new Error("late failure"));
  await tick();
  assert.equal(elements["review-message"].textContent, "new");
  elements["close-review"].click();
  assert.deepEqual(discards, ["7:1", "7:2"]);
  row.click();
  elements["close-diff"].click();
  resolveDiff({ outcome: "ready", message: "different", text: "late" });
  await tick();
  assert.equal(elements.diff.hidden, true);
  assert.equal(elements["diff-text"].textContent, "");
  assert.equal(document.activeElement, elements.message);
  assert.equal(elements.entries.hidden, true);
  elements.refresh.click();
  await tick();
  elements["nav-review"].click();
  elements["close-review"].click();
  resolveReview({ outcome: "review", message: "late", items: [{ group: "x", text: "late" }] });
  await tick();
  assert.equal(elements.review.hidden, true);
  assert.equal(elements["review-items"].children.length, 0);
  elements.refresh.click();
  await tick();
  elements["nav-review"].click();
  resolveReview({ id: null, outcome: "unsupported", message: "not supported", items: [], commit_message: "not editable" });
  await tick();
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements["commit-editor"].hidden, true);
  elements["close-review"].click();

  elements["nav-review"].click();
  resolveReview({ id: "partial", outcome: "partial", message: "partial", items: [], commit_message: "not editable" });
  await tick();
  assert.equal(elements["commit-editor"].hidden, true);
  elements["close-review"].click();

  const suggestion = "<script>safe</script>\nsecond line";
  elements["nav-review"].click();
  resolveReview({ id: "edit-id", outcome: "review", message: "loaded", items: [], commit_message: suggestion });
  await tick();
  assert.equal(elements["commit-editor"].hidden, false);
  assert.equal(elements["commit-message"].value, suggestion);
  assert.equal(elements["commit-message"].textContent, "");
  for (const invalid of [" ", "a\0b", "a\u202eb", "a".repeat(1025), "é".repeat(513), "1\n2\n3\n4\n5\n6\n7"]) {
    elements["commit-message"].value = invalid;
    elements["commit-message"].listeners.input();
    assert.equal(elements["sync-reviewed"].disabled, true);
    assert.notEqual(elements["commit-error"].textContent, "");
    elements["sync-reviewed"].click();
    assert.equal(writes.length, 0);
  }
  const draft = "--option\n<literal> é\ntrailing space ";
  elements["commit-message"].value = draft;
  elements["commit-message"].listeners.input();
  assert.equal(elements["sync-reviewed"].disabled, false);
  elements["sync-reviewed"].click();
  assert.equal(writes.length, 1);
  assert.equal(writes[0].id, "edit-id");
  assert.deepEqual(Object.keys(writes[0]), ["id", "progress", "dark", "commitMessage"]);
  assert.equal(writes[0].commitMessage, draft);
  assert.equal(elements["commit-editor"].hidden, true);
  assert.equal(elements["commit-message"].value, "");
  elements["commit-message"].value = "late edit";
  writes[0].progress.onmessage("pull");
  assert.equal(writes[0].commitMessage, draft);
  resolveWriter({ outcome: "declined", message: "no", items: [], refresh: false });
  await tick();
  elements.refresh.click();
  await tick();
  assert.equal(elements["commit-message"].value, "");
  elements["nav-review"].click();
  resolveReview({ id: "default-id", outcome: "review", message: "loaded", items: [], commit_message: suggestion });
  await tick();
  elements["sync-reviewed"].click();
  assert.deepEqual(Object.keys(writes.at(-1)), ["id", "progress", "dark"]);
  resolveWriter({ outcome: "declined", message: "no", items: [], refresh: false });
  await tick();
  elements.refresh.click();
  await tick();
  elements["nav-review"].click();
  resolveReview({ id: "clear-id", outcome: "review", message: "loaded", items: [], commit_message: "original" });
  await tick();
  elements["commit-message"].value = "discard me";
  elements["close-review"].click();
  assert.equal(elements["commit-message"].value, "");
  elements["nav-review"].click();
  resolveReview({ id: "next-id", outcome: "review", message: "loaded", items: [], commit_message: "" });
  await tick();
  assert.equal(elements["commit-editor"].hidden, true);
  assert.equal(elements["commit-message"].value, "");
  elements["close-review"].click();
  elements["nav-review"].click();
  resolveReview({ id: "refresh-id", outcome: "review", message: "loaded", items: [], commit_message: "fresh" });
  await tick();
  assert.equal(elements["commit-message"].value, "fresh");
  elements.refresh.click();
  await tick();
  assert.equal(elements["commit-editor"].hidden, true);
  assert.equal(elements["commit-message"].value, "");

  async function startWriter(id) {
    if (!elements.entries.hidden) {
      elements["nav-review"].click();
      resolveReview({ id, outcome: "review", message: "loaded", items: [] });
      await tick();
    }
    elements["sync-reviewed"].click();
    elements["sync-reviewed"].click();
    assert.equal(writes.length, idsWritten.length + 1);
    idsWritten.push(id);
    assert.equal(writes.at(-1).id, id);
    assert.deepEqual(Object.keys(writes.at(-1)), ["id", "progress", "dark"]);
    assert.equal(elements["sync-reviewed"].hidden, true);
    assert.equal(getWorkspaceView().busy, true);
    assert.equal(elements["close-review"].disabled, true);
    assert.equal(document.activeElement, elements["review-message"]);
    assert.match(elements["review-message"].textContent, /Waiting for native confirmation/);
  }
  const idsWritten = ["edit-id", "default-id"];
  await startWriter("cancel-id");
  const firstProgress = writes.at(-1).progress;
  for (const [stage, label] of [
    ["started", "Sync process started."],
    ["capture", "Sync stage reached: capturing changes."],
    ["commit", "Sync stage reached: committing changes."],
    ["pull", "Sync stage reached: pulling changes."],
    ["apply", "Sync stage reached: applying changes."],
    ["reencrypt", "Sync stage reached: reencrypting secrets."],
    ["hooks", "Sync stage reached: checking the no-hooks policy."],
    ["push", "Sync stage reached: pushing changes."],
    ["blocked_by_repo_hold", "Sync stage reached: blocked by a repository hold."],
  ]) {
    firstProgress.onmessage(stage);
    assert.equal(elements["review-message"].textContent, label);
  }
  for (const invalid of ["<script>", "toString", null, { stage: "pull" }, ["pull"]]) {
    firstProgress.onmessage(invalid);
    assert.equal(elements["review-message"].textContent, "Sync stage reached: blocked by a repository hold.");
  }
  const beforeCancel = refreshes;
  delayCancel = true;
  elements.cancel.click();
  firstProgress.onmessage("pull");
  assert.match(elements["review-message"].textContent, /Cancelling sync/);
  resolveWriter({ outcome: "cancelled", message: "reaped", items: [{ group: "Result", text: "<cancelled>" }], refresh: false });
  await tick();
  firstProgress.onmessage("push");
  assert.match(elements["review-message"].textContent, /waiting for the result/);
  assert.equal(getWorkspaceView().busy, true);
  resolveCancel();
  delayCancel = false;
  await tick();
  assert.equal(elements["review-message"].textContent, "reaped");
  firstProgress.onmessage("push");
  assert.equal(elements["review-message"].textContent, "reaped");
  assert.equal(elements["review-items"].children[0].children[1].children[0].textContent, "<cancelled>");
  assert.equal(refreshes, beforeCancel);
  assert.equal(elements.entries.hidden, true);

  elements.refresh.click();
  await tick();
  await startWriter("cancel-ack-id");
  const cancelledProgress = writes.at(-1).progress;
  elements.cancel.click();
  await tick();
  cancelledProgress.onmessage("pull");
  assert.match(elements["review-message"].textContent, /Cancelling sync/);
  resolveWriter({ outcome: "cancelled", message: "reaped", items: [], refresh: false });
  await tick();
  assert.equal(elements["review-message"].textContent, "reaped");

  for (const outcome of ["declined", "error", "done", "terminal_recovery", "cleanup_unresolved"]) {
    elements.refresh.click();
    await tick();
    await startWriter(`${outcome}-id`);
    firstProgress.onmessage("pull");
    assert.match(elements["review-message"].textContent, /Waiting for native confirmation/);
    const progress = writes.at(-1).progress;
    progress.onmessage("pull");
    assert.equal(elements["review-message"].textContent, "Sync stage reached: pulling changes.");
    const before = refreshes;
    resolveWriter({ outcome, message: "actual result", items: [{ group: "<group>", text: "<item>", diff_id: "summary-id" }], refresh: outcome === "done" || outcome === "terminal_recovery" || outcome === "cleanup_unresolved" });
    await tick();
    assert.equal(refreshes, before + (outcome === "done" || outcome === "declined" ? 1 : 0));
    assert.match(elements["review-message"].textContent, /^actual result/);
    progress.onmessage("push");
    assert.match(elements["review-message"].textContent, /^actual result/);
    assert.equal(elements.review.hidden, false);
    assert.equal(elements.entries.hidden, outcome !== "done" && outcome !== "declined");
    assert.equal(elements["review-items"].children[0].children[0].textContent, "<group>");
    assert.equal(elements["review-items"].children[0].children[1].children[0].textContent, "<item>");
    assert.equal(elements["review-items"].children[0].children[1].children[0].children.length, 0);
    if (outcome === "terminal_recovery" || outcome === "cleanup_unresolved") {
      assert.match(elements["review-message"].textContent, /terminal/);
    }
    if (outcome === "terminal_recovery") {
      assert.equal(elements.recovery.hidden, false);
      assert.equal(elements["recovery-message"].textContent, "actual result");
      assert.equal(elements["recovery-steps"].children.length, 0);
    }
  }
  elements.refresh.click();
  await tick();
  await startWriter("recovery-id");
  const beforeRecovery = refreshes;
  const beforeCopies = copies.length;
  resolveWriter({
    outcome: "terminal_recovery", message: "<terminal message>", items: [], refresh: true,
    recovery: [
      { kind: "instruction", text: "<script>instruction</script>" },
      { kind: "command", condition: "if <first>", shell: "dotman git 'one <two>'" },
      { kind: "instruction", text: "" },
      { kind: "command", condition: "", shell: "dotman git --abort\n" },
      { kind: "command", condition: null, shell: "" },
    ],
  });
  await tick();
  assert.equal(refreshes, beforeRecovery);
  assert.equal(copies.length, beforeCopies);
  assert.equal(elements["recovery-message"].textContent, "<terminal message>");
  const steps = elements["recovery-steps"].children;
  assert.equal(steps.length, 5);
  assert.equal(steps[0].textContent, "<script>instruction</script>");
  assert.equal(steps[1].children[0].textContent, "if <first>");
  assert.equal(steps[1].children[1].textContent, "dotman git 'one <two>'");
  assert.equal(steps[1].children[1].attributes["aria-label"], "Recovery command 2");
  assert.equal(steps[2].textContent, "");
  assert.equal(steps[3].children[0].textContent, "");
  assert.equal(steps[3].children[1].textContent, "dotman git --abort\n");
  assert.equal(steps[4].children[0].textContent, "");
  const beforeClickCalls = calls.length;
  copyFails = true;
  await steps[1].children[2].click();
  assert.equal(copies.at(-1), "dotman git 'one <two>'");
  assert.match(steps[1].children[3].textContent, /select.*manually/i);
  copyFails = false;
  await steps[3].children[2].click();
  assert.equal(copies.at(-1), "dotman git --abort\n");
  assert.equal(steps[3].children[3].textContent, "Command copied.");
  assert.equal(copies.length, beforeCopies + 2);
  assert.equal(calls.length, beforeClickCalls);
  elements["close-review"].click();
  assert.equal(elements.recovery.hidden, false);
  statusResponse = { ...status, success: false, message: "operation in progress", entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["recovery-steps"].children[1].children[1].textContent, "dotman git 'one <two>'");
  statusResponse = { ...status, success: false, repo_unavailable: true, entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.setup.hidden, true);
  statusResponse = null;
  const beforeSuccessCalls = calls.length;
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, true);
  assert.equal(elements["recovery-steps"].children.length, 0);
  assert.deepEqual(calls.slice(beforeSuccessCalls), ["refresh"]);
  assert.equal(copies.length, beforeCopies + 2);

  elements.refresh.click();
  await tick();
  await startWriter("stale-id");
  const beforeFailure = refreshes;
  rejectWriter(new Error("stale"));
  await tick();
  assert.equal(refreshes, beforeFailure);
  assert.match(elements["review-message"].textContent, /failed.*terminal/);

  async function captureReview(outcome = "review", id = "review-capture") {
    elements.refresh.click();
    await tick();
    elements["nav-review"].click();
    resolveReview({ id, outcome, message: "review", commit_message: "suggested", items: [
      { group: "Capture", text: "<script>literal</script>", diff_id: "--opaque <id>" },
      { group: "Capture", text: "unavailable", diff_id: null },
    ] });
    await tick();
    return elements["review-items"].children[0].children[1].children;
  }
  let captureRows = await captureReview("partial");
  assert.equal(captureRows[0].children.length, 0);
  assert.equal(captureRows[1].children.length, 0);
  elements["close-review"].click();
  captureRows = await captureReview();
  const captureButton = captureRows[0].children[0];
  assert.equal(captureRows[0].textContent, "<script>literal</script>");
  assert.equal(captureRows[1].children.length, 0);
  assert.equal(captureButton.textContent, "View capture diff");
  assert.equal(captureButton.attributes["aria-label"], "View capture diff: <script>literal</script>");
  elements["commit-message"].value = "unsent draft";
  elements["commit-message"].listeners.input();
  window.scrollY = 4321;
  captureButton.click();
  assert.equal(elements.review.hidden, true);
  assert.equal(document.activeElement, elements["close-diff"]);
  assert.equal(elements["next-undecided"].hidden, true);
  assert.deepEqual(Object.keys(diffRequests.at(-1)), ["id"]);
  assert.equal(diffRequests.at(-1).id, "--opaque <id>");
  assert.equal(elements["diff-key"].textContent, "<script>literal</script>");
  assert.equal(elements["diff-context"].textContent, "Capture preview");
  resolveDiff({ outcome: "ready", message: "ready", text: "<img src=x>\nsecret", file_actions: ["keep_local"] });
  await tick();
  assert.equal(elements["diff-text"].textContent, "<img src=x>\nsecret");
  assert.equal(elements["diff-actions"].children.length, 0);
  elements["close-diff"].click();
  assert.equal(elements["diff-text"].textContent, "");
  assert.equal(elements.review.hidden, false);
  assert.equal(document.activeElement, captureButton);
  assert.deepEqual(window.restored, [0, 4321]);
  assert.equal(elements["commit-message"].value, "unsent draft");
  assert.equal(elements["sync-reviewed"].hidden, false);
  captureButton.click();
  rejectDiff(new Error("ordinary failure"));
  await tick();
  assert.equal(elements["diff-message"].textContent, "Diff read failed.");
  elements["close-diff"].click();
  assert.equal(elements["commit-message"].value, "unsent draft");
  assert.equal(document.activeElement, captureButton);
  elements["close-review"].click();
  assert.equal(document.activeElement, elements["nav-review"]);
  assert.deepEqual(window.restored, [0, 1234]);
  captureRows = await captureReview();
  const recoveryButton = captureRows[0].children[0];
  elements["commit-message"].value = "discard on recovery";
  recoveryButton.click();
  resolveDiff({ outcome: "terminal_recovery", message: "terminal needed", text: null });
  await tick();
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements["commit-message"].value, "");
  assert.equal(elements["review-items"].children.length, 0);
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["recovery-message"].textContent, "terminal needed");
  elements["close-diff"].click();
  assert.equal(elements.review.hidden, true);
  assert.equal(document.activeElement, elements.message);
  const countAfterRecovery = diffRequests.length;
  recoveryButton.click();
  assert.equal(diffRequests.length, countAfterRecovery);

  captureRows = await captureReview();
  const cancelledButton = captureRows[0].children[0];
  elements["commit-message"].value = "discard on cancel";
  cancelledButton.click();
  elements.cancel.click();
  rejectDiff(new Error("late failure"));
  await tick();
  assert.equal(elements.diff.hidden, true);
  assert.equal(elements.review.hidden, true);
  assert.equal(elements["commit-message"].value, "");
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements["review-items"].children.length, 0);
  assert.equal(document.activeElement, elements.message);
  const countAfterCancel = diffRequests.length;
  cancelledButton.click();
  assert.equal(diffRequests.length, countAfterCancel);
  captureRows = await captureReview();
  const refreshButton = captureRows[0].children[0];
  refreshButton.click();
  resolveDiff({ outcome: "ready", message: "loaded", text: "private" });
  await tick();
  elements.refresh.click();
  await tick();
  assert.equal(elements.diff.hidden, true);
  assert.equal(elements["diff-text"].textContent, "");
  assert.equal(elements.review.hidden, true);
  assert.equal(elements["commit-message"].value, "");
  const countAfterRefresh = diffRequests.length;
  refreshButton.click();
  assert.equal(diffRequests.length, countAfterRefresh);
  statusResponse = { ...status, entries: [
    { id: "first", key: "same", status: "conflict", group: "decision", supported: true },
    { id: "held-next", key: "held", status: "held", group: "held", supported: true },
    { id: "system-next", key: "system", status: "conflict", group: "decision", supported: true, system: true },
    { id: "other-next", key: "other", status: "changed", group: "other", supported: true },
    { id: "second", key: "same", status: "deleted", group: "decision", supported: true },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements["guide-title"].textContent, "File decisions");
  assert.match(elements["guide-description"].textContent, /2 to inspect/);
  assert.equal(elements["attention-list"].children.length, 1);
  const beforeNavigationCalls = calls.length;
  const beforeNavigationWrites = fileWrites.length;
  elements["attention-list"].children[0].children[0].click();
  assert.equal(diffRequests.at(-1).id, "first");
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements.search.value = "not found";
  elements.search.listeners.input();
  assert.equal(elements["next-undecided"].hidden, false);
  elements["next-undecided"].click();
  assert.equal(diffRequests.at(-1).id, "second");
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: [] });
  await tick();
  assert.equal(elements["next-undecided"].hidden, true);
  elements["close-diff"].click();
  assert.equal(document.activeElement, elements.search);
  elements["clear-decisions"].click();
  elements.search.value = "";
  elements.search.listeners.input();
  assert.deepEqual(calls.slice(beforeNavigationCalls), ["open_diff", "open_diff"]);
  assert.equal(fileWrites.length, beforeNavigationWrites);
  const allText = node => [node.textContent, ...node.children.flatMap(allText)];
  assert.equal(allText(elements["attention-list"]).some(text => /\bstage\b/i.test(text)), false);
  const descendants = node => [node, ...node.children.flatMap(descendants)];
  for (const key of ["system", "other"]) {
    const button = descendants(elements.rows).find(node => node.tag === "button" && node.title === key);
    assert.ok(button);
    button.click();
    resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local", "take_repo"] });
    await tick();
    assert.equal(elements["diff-actions"].children.length, 0);
    assert.equal(elements["decision-items"].children.length, 0);
    elements["close-diff"].click();
  }
  assert.equal(fileWrites.length, beforeNavigationWrites);
  assert.deepEqual(calls.slice(beforeNavigationCalls), ["open_diff", "open_diff", "open_diff", "open_diff"]);
  statusResponse = { ...status, entries: [
    { id: "opaque-a", key: "same <path>", status: "conflict", group: "decision", supported: true },
    { id: "opaque-b", key: "same <path>", status: "deleted", group: "decision", supported: true },
    { id: "opaque-c", key: "held", status: "held", group: "held", supported: false },
  ] };
  elements.refresh.click();
  await tick();
  const fileRows = elements.rows.children[0].children[1].children.map(item => item.children[0]);
  fileRows[0].click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local", "take_repo", "unknown"] });
  await tick();
  assert.equal(elements["diff-actions"].children.length, 3);
  assert.equal(elements["diff-actions"].children[2].hidden, true);
  elements["diff-actions"].children[0].click();
  assert.equal(elements["diff-actions"].children[2].hidden, false);
  assert.equal(elements.decisions.hidden, true);
  assert.notEqual(getWorkspaceView().queued, null);
  assert.equal(elements["diff-actions"].children[0].attributes["aria-pressed"], "true");
  elements["diff-actions"].children[1].click();
  assert.equal(elements["diff-actions"].children[0].attributes["aria-pressed"], "false");
  assert.equal(elements["diff-actions"].children[1].attributes["aria-pressed"], "true");
  elements["diff-actions"].children[2].click();
  assert.equal(elements["diff-actions"].children[2].hidden, true);
  assert.equal(document.activeElement, elements["diff-actions"].children[1]);
  assert.equal(elements.decisions.hidden, true);
  assert.equal(elements["diff-actions"].children[1].attributes["aria-pressed"], "false");
  elements["diff-actions"].children[1].click();
  assert.equal(elements["decision-items"].children.length, 1);
  assert.equal(elements["decision-items"].children[0].children[0].textContent, "same <path>");
  assert.equal(fileWrites.length, 0);
  elements["close-diff"].click();
  fileRows[1].click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["restore"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements["diff-actions"].children[1].click();
  assert.equal(elements["diff-selection"].textContent, "");
  assert.equal(elements["decision-items"].children.length, 1);
  elements["diff-actions"].children[0].click();
  elements["close-diff"].click();
  assert.equal(elements["decision-items"].children.length, 2);
  fileRows[0].click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["take_repo"] });
  await tick();
  assert.equal(elements["diff-actions"].children[0].attributes["aria-pressed"], "true");
  elements["close-diff"].click();
  elements["nav-review"].click();
  resolveReview({ id: "queue-capture", outcome: "review", message: "review", items: [
    { group: "Capture", text: "capture", diff_id: "opaque-a" },
  ] });
  await tick();
  elements["review-items"].children[0].children[1].children[0].children[0].click();
  rejectDiff(new Error("capture failed"));
  await tick();
  assert.equal(elements["decision-items"].children.length, 2);
  elements["close-diff"].click();
  elements["review-items"].children[0].children[1].children[0].children[0].click();
  resolveDiff({ outcome: "ready", message: "capture", text: "", file_actions: ["keep_local"] });
  await tick();
  assert.equal(elements["decision-items"].children.length, 2);
  assert.equal(elements["diff-actions"].children.length, 0);
  elements["close-diff"].click();
  elements["close-review"].click();
  for (const response of [
    { outcome: "ready", message: "no actions", text: "" },
    { outcome: "ready", message: "unknown", text: "", file_actions: ["unknown"] },
    { outcome: "unsupported", message: "no", text: "", file_actions: ["take_repo"] },
    { outcome: "ready", message: "changed actions", text: "", file_actions: ["keep_local"] },
    null,
  ]) {
    fileRows[0].click();
    if (response) resolveDiff(response); else rejectDiff(new Error("failed"));
    await tick();
    assert.equal(elements["decision-items"].children.length, 1);
    assert.equal(elements["decision-items"].children[0].children[0].textContent, "same <path>");
    elements["close-diff"].click();
    fileRows[0].click();
    resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["take_repo"] });
    await tick();
    elements["diff-actions"].children[0].click();
    elements["close-diff"].click();
  }
  elements["decision-items"].children[1].children[2].click();
  assert.equal(elements["decision-items"].children.length, 1);
  fileRows[0].click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["take_repo"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements["close-diff"].click();
  elements["apply-decisions"].click();
  elements["apply-decisions"].click();
  assert.equal(fileWrites.length, 1);
  assert.equal(JSON.stringify(fileWrites[0]), JSON.stringify({ answers: [
    { id: "opaque-b", action: "restore" }, { id: "opaque-a", action: "take_repo" },
  ] }));
  assert.equal(elements["decision-items"].children.length, 0);
  delayCancel = true;
  elements.cancel.click();
  assert.match(elements["decision-message"].textContent, /Cancelling apply/);
  const beforeFileRefresh = refreshes;
  resolveWriter({ outcome: "partial", message: "some stale", items: [{ group: "Result", text: "<literal>" }], refresh: true });
  await tick();
  assert.match(elements["decision-message"].textContent, /waiting/);
  resolveCancel();
  delayCancel = false;
  await tick();
  assert.equal(refreshes, beforeFileRefresh + 1);
  assert.equal(elements["decision-message"].textContent, "some stale");
  assert.equal(elements.decisions.hidden, false);
  assert.equal(elements["decision-results"].children[0].textContent, "Result: <literal>");
  const freshRow = elements.rows.children[0].children[1].children[0].children[0];
  freshRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "" });
  await tick();
  assert.equal(elements["diff-actions"].children.length, 0);
  elements["close-diff"].click();
  freshRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  const staleChoice = elements["diff-actions"].children[0];
  staleChoice.click();
  elements.refresh.click();
  await tick();
  staleChoice.click();
  assert.equal(elements["decision-items"].children.length, 0);
  assert.equal(elements.decisions.hidden, true);
  const heldRow = elements.rows.children[1].children[1].children[0].children[0];
  assert.equal(heldRow.className, "held-row");
  heldRow.open = true;
  heldRow.listeners.toggle();
  assert.equal(heldRow.children[1].textContent, "Origin: Unknown");
  assert.equal(elements["diff-actions"].children.length, 0);
  const currentRow = elements.rows.children[0].children[1].children[0].children[0];
  currentRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  const closedChoice = elements["diff-actions"].children[0];
  closedChoice.click();
  elements["close-diff"].click();
  elements["clear-decisions"].click();
  closedChoice.click();
  assert.equal(elements["decision-items"].children.length, 0);
  currentRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements["close-diff"].click();
  elements["clear-decisions"].click();
  assert.equal(elements["decision-items"].children.length, 0);
  currentRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements["close-diff"].click();
  currentRow.click();
  elements.cancel.click();
  resolveDiff({ outcome: "ready", message: "late", text: "", file_actions: ["restore"] });
  await tick();
  assert.equal(elements["decision-items"].children.length, 0);
  assert.equal(elements["diff-actions"].children.length, 0);
  elements.refresh.click();
  await tick();
  const nextRow = elements.rows.children[0].children[1].children[0].children[0];
  nextRow.click();
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["restore"] });
  await tick();
  elements["diff-actions"].children[0].click();
  elements["close-diff"].click();
  elements["apply-decisions"].click();
  const beforeFileRecovery = refreshes;
  resolveWriter({ outcome: "terminal_recovery", message: "recover", items: [], refresh: true, recovery: [] });
  await tick();
  assert.equal(refreshes, beforeFileRecovery);
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["decision-message"].textContent, "recover");
  statusResponse = { ...status, held_counts: { new_files: 2, home_edits: 1, repo_edits: 1 }, entries: [
    { id: "new1", key: "<redacted>", status: "held", group: "held", supported: true,
      hold: { origin: "new_file", reason: "<reason>", rule: "<rule>", location: "line 7", guidance: "<terminal guidance>" } },
    { id: "new2", key: "<redacted>", status: "held", group: "held", supported: true,
      hold: { origin: "new_file", reason: "second", guidance: "Use a terminal." } },
    { id: "home", key: "home", status: "held", group: "held", hold: { origin: "home_edit", reason: "home reason" } },
    { id: "repo", key: "repo", status: "held", group: "held", hold: { origin: "repo_edit", reason: "repo reason" } },
    { id: "unknown", key: "unknown", status: "held", group: "held", supported: true, hold: { origin: "toString", reason: 42, rule: {}, location: null } },
    { id: "missing", key: "missing", status: "held", group: "held", supported: true },
    { id: "ordinary", key: "ordinary", status: "changed", group: "outgoing", supported: true },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements["held-counts"].hidden, false);
  assert.equal(elements["held-counts"].textContent, "Held in full report: new files 2 · home edits 1 · repository edits 1");
  let holds = elements.rows.children[1].children[1].children.map(item => item.children[0]);
  assert.equal(holds.length, 6);
  for (const hold of holds) {
    assert.equal(hold.children.length, 1);
    hold.open = true;
    hold.listeners.toggle();
    hold.listeners.toggle();
  }
  assert.equal(holds[0].children[0].children[1].children[0].textContent, "<redacted>");
  assert.equal(holds[1].children[0].children[1].children[0].textContent, "<redacted>");
  assert.deepEqual(holds[0].children.slice(1).map(item => item.textContent), [
    "Origin: New file", "Reason: <reason>", "Rule: <rule>", "Location: line 7", "Terminal guidance: <terminal guidance>",
  ]);
  assert.equal(holds[1].children[2].textContent, "Reason: second");
  assert.equal(holds[2].children[1].textContent, "Origin: Home edit");
  assert.equal(holds[3].children[1].textContent, "Origin: Repository edit");
  assert.deepEqual(holds[4].children.slice(1).map(item => item.textContent), ["Origin: Unknown", "Reason: Unavailable"]);
  assert.deepEqual(holds[5].children.slice(1).map(item => item.textContent), ["Origin: Unknown", "Reason: Unavailable"]);
  const beforeHeldCalls = calls.length;
  const beforeHeldCopies = copies.length;
  for (const hold of holds) {
    assert.equal(hold.children[0].listeners.click, undefined);
    assert.equal(hold.children.slice(1).every(item => item.children.length === 0), true);
  }
  assert.equal(calls.length, beforeHeldCalls);
  assert.equal(copies.length, beforeHeldCopies);
  elements.search.value = "home";
  elements.search.listeners.input();
  assert.equal(elements["held-counts"].textContent, "Held in full report: new files 2 · home edits 1 · repository edits 1");
  holds = elements.rows.children[0].children[1].children.map(item => item.children[0]);
  assert.equal(holds.length, 1);
  holds[0].open = true;
  holds[0].listeners.toggle();
  assert.equal(holds[0].children[1].textContent, "Origin: Home edit");
  elements.search.value = "";
  elements.search.listeners.input();
  elements.rows.children[0].children[1].children[0].children[0].click();
  assert.equal(diffRequests.at(-1).id, "ordinary");
  resolveDiff({ outcome: "ready", message: "ready", text: "", file_actions: ["keep_local"] });
  await tick();
  assert.equal(elements["diff-actions"].children.length, 2);
  elements["close-diff"].click();
  statusResponse = { ...status, held_counts: { new_files: 1 }, entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements["held-counts"].textContent, "Held in full report: new files 1 · home edits unknown · repository edits unknown");
  statusResponse = status;
  elements.refresh.click();
  await tick();
  assert.equal(elements["held-counts"].hidden, true);
  statusResponse = { ...status, entries: [
    { id: "7:1", key: "safe", status: "held", group: "held", held_actions: ["encrypt", "forget"] },
    { id: "7:2", key: "risky", status: "held", group: "held", held_actions: [] },
  ] };
  elements.refresh.click();
  await tick();
  const actionHolds = elements.rows.children[0].children[1].children.map(item => item.children[0]);
  for (const hold of actionHolds) { hold.open = true; hold.listeners.toggle(); }
  const buttons = actionHolds[0].children.filter(child => child.tag === "button");
  assert.deepEqual(buttons.map(button => button.textContent), ["Encrypt", "Forget"]);
  assert.ok(buttons.every(button => button.type === "button"));
  assert.equal(actionHolds[1].children.some(child => child.tag === "button"), false);
  buttons[0].focus();
  assert.equal(document.activeElement, buttons[0]);
  buttons[0].click();
  assert.deepEqual(heldWrites.at(-1), { id: "7:1", action: "encrypt" });
  buttons[1].click();
  assert.equal(heldWrites.length, 1);
  resolveWriter({ outcome: "declined", message: "Not approved", refresh: true });
  await tick();
  assert.equal(elements["identity-message"].textContent, "declined: Not approved");
  statusResponse = status;
  elements.refresh.click();
  await tick();
  const readOnlyCalls = calls.length;
  const readOnlyCopies = copies.length;
  const readOnlyWrites = writes.length;
  const readOnlyFileWrites = fileWrites.length;
  statusResponse = { ...status, success: false, repo_unavailable: true, entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.setup.hidden, false);
  assert.equal(elements.context.hidden, false);
  assert.equal(elements.paths.children[3].textContent, "/r");
  assert.equal(elements["setup-paths"].children[3].textContent, "/r");
  assert.equal(elements["start-fresh"].hidden, true);
  assert.match(elements["setup-guidance"].textContent, /not available/);
  const checkCalls = calls.length;
  delayRefresh = true;
  elements["setup-check"].click();
  assert.equal(elements["setup-check"].disabled, true);
  assert.equal(elements["start-fresh"].hidden, true);
  resolveRefresh({ ...status, success: false, repo_unavailable: true, finding: "unavailable", setup_review_id: "setup-check", entries: [] });
  delayRefresh = false;
  await tick();
  assert.deepEqual(calls.slice(checkCalls), ["refresh"]);
  assert.equal(elements["start-fresh"].hidden, false);
  assert.match(elements["setup-guidance"].textContent, /Create, Clone, or Import/);
  delayRefresh = true;
  elements["setup-check"].click();
  rejectRefresh(new Error("private stderr"));
  delayRefresh = false;
  await tick();
  assert.equal(elements.setup.hidden, false);
  assert.equal(elements["start-fresh"].hidden, true);
  assert.equal(elements["setup-check"].disabled, false);
  assert.equal(getWorkspaceView().status, "Status read failed.");
  for (const response of [
    { ...status, success: false, repo_unavailable: false, message: "guard", entries: [] },
    { ...status, success: false, message: "generic", entries: [] },
    { ...status, success: false, repo_unavailable: true, context: null, entries: [] },
  ]) {
    statusResponse = response;
    elements.refresh.click();
    await tick();
    assert.equal(elements.setup.hidden, true);
  }
  statusResponse = { ...status, context: { ...status.context, machine_details: {
    host: "<host>", os: "linux", arch: "x86_64", profiles: ["work", "<profile>"],
  } }, entries: [
    { id: "clean", key: "<clean>", status: "clean", group: "clean", machine_file: {
      selected: "<selected>", recorded: "<recorded>", alternatives: ["<other>", "second"],
      kind: "template", identity_changed: true, ambiguous: true,
    } },
    { id: "changed", key: "changed", status: "changed", group: "outgoing", supported: true,
      machine_file: { selected: "encrypted.age", recorded: null, alternatives: [], kind: "encrypted", identity_changed: false, ambiguous: false } },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.setup.hidden, true);
  assert.equal(elements["machine-files"].hidden, false);
  assert.equal(elements["machine-file-details"].children.length, 0);
  elements["machine-files"].open = true;
  elements["machine-files"].listeners.toggle();
  const machineRoot = elements["machine-file-details"];
  assert.deepEqual(machineRoot.children[0].children.map(item => item.textContent),
    ["Host", "<host>", "OS", "linux", "Architecture", "x86_64", "Profiles", "work, <profile>"]);
  const machineRows = machineRoot.children[1].children;
  assert.equal(machineRows.length, 2);
  assert.equal(machineRows[0].children[0].textContent, "<clean>");
  assert.deepEqual(machineRows[0].children.slice(1, 5).map(line => [line.textContent, line.children[0].textContent]),
    [["Selected source: ", "<selected>"], ["Recorded source: ", "<recorded>"],
      ["Other reported sources: ", "<other>"], ["Other reported sources: ", "second"]]);
  assert.match(machineRows[0].children[5].textContent, /template.*Ambiguous: yes.*Identity changed: yes/);
  assert.match(machineRows[1].children.at(-1).textContent, /encrypted.*Ambiguous: no.*Identity changed: no/);
  statusResponse = { ...statusResponse, entries: [statusResponse.entries[1]] };
  elements.refresh.click();
  await tick();
  assert.equal(machineRoot.children[1].children.length, 1);
  assert.equal(machineRoot.children[1].children[0].children[0].textContent, "changed");
  elements["nav-review"].click();
  resolveReview({ outcome: "terminal_recovery", id: null, message: "recovery", items: [] });
  await tick();
  assert.equal(elements["machine-files"].hidden, true);
  assert.equal(machineRoot.children.length, 0);
  elements["close-review"].click();
  elements.refresh.click();
  await tick();
  assert.equal(machineRoot.children[1].children.length, 1);
  statusResponse = { ...status, success: false, repo_unavailable: true, entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.setup.hidden, false);
  assert.equal(elements["machine-files"].hidden, true);
  assert.equal(machineRoot.children.length, 0);
  elements["machine-files"].open = false;
  statusResponse = { ...status, entries: Array.from({ length: 2000 }, (_, index) => ({
    id: String(index), key: `file-${index}`, status: "changed", group: "outgoing", supported: true,
    machine_file: { selected: "source", recorded: null, alternatives: ["alternative"], kind: "plain", identity_changed: false, ambiguous: false },
  })) };
  elements.refresh.click();
  await tick();
  assert.equal(elements["machine-files"].hidden, false);
  assert.equal(machineRoot.children.length, 0);
  assert.deepEqual(calls.slice(readOnlyCalls).filter(call => call !== "refresh"), ["open_review"]);
  assert.equal(copies.length, readOnlyCopies);
  assert.equal(writes.length, readOnlyWrites);
  assert.equal(fileWrites.length, readOnlyFileWrites);
  assert.equal(identityWrites.length, 0);

  for (const id of [undefined, null, "", 12, {}]) {
    statusResponse = { ...status, identity_review_id: id };
    elements.refresh.click();
    await tick();
    assert.equal(elements["review-identity"].hidden, true);
    elements["review-identity"].click();
    assert.equal(identityWrites.length, 0);
  }
  statusResponse = { ...status, success: false, identity_review_id: "invalid-on-failure", entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements["review-identity"].hidden, true);

  async function identityStatus(id) {
    elements["nav-settings"].click();
    statusResponse = { ...status, identity_review_id: id };
    const before = calls.length;
    elements.refresh.click();
    await tick();
    assert.deepEqual(calls.slice(before), ["refresh"]);
    assert.equal(elements["machine-files"].hidden, false);
    assert.equal(elements["review-identity"].hidden, false);
    elements["machine-files"].open = true;
    elements["machine-files"].listeners.toggle();
    assert.deepEqual(calls.slice(before), ["refresh"]);
  }
  await identityStatus("old-generation");
  statusResponse = { ...status, identity_review_id: null };
  elements.refresh.click();
  await tick();
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 0);

  await identityStatus("opaque <id>");
  elements["review-identity"].click();
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 1);
  assert.deepEqual(Object.keys(identityWrites[0]), ["id"]);
  assert.equal(identityWrites[0].id, "opaque <id>");
  assert.equal(elements["review-identity"].hidden, true);
  assert.equal(elements["identity-result"].hidden, false);
  assert.equal(document.activeElement, elements["identity-message"]);
  resolveWriter({ outcome: "declined", message: "native denial", items: [], refresh: false });
  await tick();
  assert.equal(elements["identity-message"].textContent, "declined: native denial");
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 1);

  await identityStatus("cancel-id");
  elements["review-identity"].click();
  delayCancel = true;
  elements.cancel.click();
  assert.match(elements["identity-message"].textContent, /Cancelling identity review/);
  resolveWriter({ outcome: "cancelled", message: "finished", items: [], refresh: false });
  await tick();
  assert.match(elements["identity-message"].textContent, /waiting/);
  resolveCancel();
  delayCancel = false;
  await tick();
  assert.equal(elements["identity-message"].textContent, "cancelled: finished");

  await identityStatus("terminal-id");
  elements["review-identity"].click();
  const beforeTerminal = refreshes;
  resolveWriter({ outcome: "terminal_recovery", message: "terminal needed", items: [], refresh: true, recovery: [] });
  await tick();
  assert.equal(refreshes, beforeTerminal);
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["identity-message"].textContent, "terminal_recovery: terminal needed");

  await identityStatus("success-id");
  elements["review-identity"].click();
  statusResponse = { ...status, identity_review_id: null };
  const beforeIdentityRefresh = refreshes;
  resolveWriter({ outcome: "done", message: "accepted", items: [], refresh: true });
  await tick();
  assert.equal(refreshes, beforeIdentityRefresh + 1);
  assert.equal(elements["identity-message"].textContent, "done: accepted");
  assert.equal(elements["identity-result"].hidden, false);
  assert.equal(elements["review-identity"].hidden, true);
  assert.equal(identityWrites.length, 4);

  await identityStatus("invalidated-by-read-cancel");
  elements.rows.children[0].children[1].children[0].children[0].click();
  elements.cancel.click();
  resolveDiff({ outcome: "ready", message: "late", text: "" });
  await tick();
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 4);
  assert.equal(elements["review-identity"].hidden, true);

  await identityStatus("invalidated-by-sync");
  elements["nav-review"].click();
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 4);
  resolveReview({ id: "sync-invalidates", outcome: "review", message: "ready", items: [] });
  await tick();
  elements["sync-reviewed"].click();
  assert.equal(elements["review-identity"].hidden, true);
  resolveWriter({ outcome: "declined", message: "no", items: [], refresh: false });
  await tick();
  elements["review-identity"].click();
  assert.equal(identityWrites.length, 4);
  const unavailable = { ...status, success: false, repo_unavailable: true, entries: [] };
  for (const response of [
    { ...unavailable }, { ...unavailable, setup_review_id: null },
    { ...unavailable, setup_review_id: "" }, { ...unavailable, setup_review_id: 4 },
    { ...unavailable, repo_unavailable: false, setup_review_id: "invalid" },
    { ...unavailable, context: null, setup_review_id: "invalid" },
    { ...unavailable, context: { ...status.context, repo: "" }, setup_review_id: "invalid" },
    { ...status, setup_review_id: "invalid" },
  ]) {
    statusResponse = response;
    elements.refresh.click();
    await tick();
    assert.equal(elements["start-fresh"].hidden, true);
    elements["start-fresh"].click();
    assert.equal(setupWrites.length, 0);
  }
  async function setupStatus(id) {
    statusResponse = { ...unavailable, setup_review_id: id };
    const before = calls.length;
    elements.refresh.click();
    await tick();
    assert.deepEqual(calls.slice(before), ["refresh"]);
    assert.equal(elements.setup.hidden, false);
    assert.equal(elements["start-fresh"].hidden, false);
    assert.equal(elements.paths.children[3].textContent, "/r");
  }
  await setupStatus("stale");
  statusResponse = unavailable;
  elements.refresh.click();
  elements["start-fresh"].click();
  await tick();
  assert.equal(elements["start-fresh"].hidden, true);
  assert.equal(setupWrites.length, 0);

  await setupStatus("opaque <setup>");
  elements["start-fresh"].click();
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 1);
  assert.deepEqual(Object.keys(setupWrites[0]), ["id"]);
  assert.equal(setupWrites[0].id, "opaque <setup>");
  assert.equal(getWorkspaceView().busy, true);
  assert.equal(elements.setup.hidden, true);
  assert.equal(document.activeElement, elements["setup-message"]);
  const beforeDecline = refreshes;
  resolveWriter({ outcome: "declined", message: "not approved", items: [], refresh: false });
  await tick();
  assert.equal(refreshes, beforeDecline);
  assert.equal(elements["setup-message"].textContent, "not approved");
  assert.equal(elements["setup-retry"].hidden, false);
  assert.equal(getWorkspaceView().status, "Repository setup cancelled.");
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 1);
  statusResponse = { ...unavailable, setup_review_id: "refreshed-setup" };
  elements["setup-retry"].focus();
  elements["setup-retry"].click();
  await tick();
  assert.equal(refreshes, beforeDecline + 1);
  assert.equal(elements.setup.hidden, false);
  assert.equal(elements["start-fresh"].hidden, false);
  assert.equal(elements["setup-retry"].hidden, true);
  assert.equal(elements["setup-result"].hidden, true);
  assert.equal(document.activeElement, elements["setup-check"]);

  await setupStatus("cancel");
  elements["start-fresh"].click();
  delayCancel = true;
  elements.cancel.click();
  assert.match(elements["setup-message"].textContent, /Cancelling setup/);
  resolveWriter({ outcome: "cancelled", message: "stopped", items: [], refresh: false });
  await tick();
  assert.match(elements["setup-message"].textContent, /waiting/);
  resolveCancel();
  delayCancel = false;
  await tick();
  assert.equal(elements["setup-message"].textContent, "stopped");
  assert.equal(elements["setup-retry"].hidden, false);
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 2);

  await setupStatus("terminal");
  elements["start-fresh"].click();
  const beforeSetupTerminal = refreshes;
  resolveWriter({ outcome: "terminal_recovery", message: "terminal needed", items: [], refresh: true, recovery: [] });
  await tick();
  assert.equal(refreshes, beforeSetupTerminal);
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["setup-message"].textContent, "terminal_recovery: terminal needed");
  statusResponse = { ...unavailable, setup_review_id: "blocked-by-recovery" };
  elements.refresh.click();
  await tick();
  assert.equal(elements.setup.hidden, true);
  assert.equal(elements["start-fresh"].hidden, true);
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 3);

  statusResponse = status;
  elements.refresh.click();
  await tick();
  await setupStatus("verified");
  elements["start-fresh"].click();
  statusResponse = status;
  const beforeSetupSuccess = refreshes;
  resolveWriter({ outcome: "done", message: "verified", items: [], refresh: true });
  await tick();
  assert.equal(refreshes, beforeSetupSuccess + 1);
  assert.equal(elements.setup.hidden, true);
  assert.equal(elements["setup-message"].textContent, "done: verified");
  assert.equal(elements["setup-result"].hidden, false);
  assert.equal(setupWrites.length, 4);

  await setupStatus("import-review");
  elements["start-import"].click();
  elements["start-import"].click();
  assert.equal(importWrites.length, 1);
  assert.deepEqual(importWrites[0], { id: "import-review", action: "import" });
  resolveWriter({ outcome: "declined", message: "not approved", items: [], refresh: false });
  await tick();

  await setupStatus("cancel-read");
  delayRefresh = true;
  elements.refresh.click();
  elements.cancel.click();
  resolveRefresh({ ...unavailable, setup_review_id: "late" });
  delayRefresh = false;
  await tick();
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 4);
  assert.equal(elements["start-fresh"].hidden, true);
  await setupStatus("uncertain");
  elements["start-fresh"].click();
  const beforeUnknown = refreshes;
  resolveWriter({ outcome: "unknown", message: "Repository may already exist.", items: [{ text: "raw output" }], refresh: false });
  await tick();
  assert.equal(refreshes, beforeUnknown);
  assert.equal(elements["setup-message"].textContent, "unknown: Repository may already exist.");
  assert.equal(elements["setup-message"].children.length, 0);
  elements["start-fresh"].click();
  assert.equal(setupWrites.length, 5);
  await setupStatus("failed");
  elements["start-fresh"].click();
  rejectWriter(new Error("private stderr"));
  await tick();
  assert.match(elements["setup-message"].textContent, /may already exist/);
  assert.doesNotMatch(elements["setup-message"].textContent, /private stderr/);
  const beforeCloneCalls = calls.length;
  elements["clone-source"].value = "unused";
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 0);
  await setupStatus("fresh-shared");
  assert.equal(elements["clone-controls"].hidden, false);
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["start-clone"].disabled, true);
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 0);
  elements["clone-source"].value = "file:///example/source";
  elements["clone-source"].listeners.input();
  assert.equal(elements["start-clone"].disabled, false);
  elements["start-fresh"].click();
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["clone-controls"].hidden, true);
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 0);
  resolveWriter({ outcome: "declined", message: "no", items: [], refresh: false });
  await tick();

  await setupStatus("clone-shared");
  const literal = "file:///synthetic/a+b,=repo";
  elements["clone-source"].value = literal;
  elements["start-clone"].click();
  elements["start-clone"].click();
  elements["start-fresh"].click();
  assert.equal(cloneWrites.length, 1);
  assert.deepEqual(Object.keys(cloneWrites[0]), ["id", "source"]);
  assert.equal(cloneWrites[0].id, "clone-shared");
  assert.equal(cloneWrites[0].source, literal);
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["clone-controls"].hidden, true);
  assert.equal(elements["start-fresh"].hidden, true);
  resolveWriter({ outcome: "declined", message: "not approved", items: [], refresh: false });
  await tick();
  assert.equal(elements["setup-message"].textContent, "not approved");
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 1);

  await setupStatus("clone-stale");
  elements["clone-source"].value = "file:///old";
  statusResponse = unavailable;
  elements.refresh.click();
  assert.equal(elements["clone-source"].value, "");
  elements["start-clone"].click();
  await tick();
  assert.equal(elements["clone-controls"].hidden, true);
  assert.equal(cloneWrites.length, 1);

  await setupStatus("too-long");
  elements["clone-source"].value = "a".repeat(8193);
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 1);
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["clone-controls"].hidden, true);
  assert.match(elements["setup-message"].textContent, /8192/);

  await setupStatus("uncertain-clone");
  elements["clone-source"].value = " https://example.invalid/a ";
  elements["start-clone"].click();
  assert.equal(cloneWrites[1].source, " https://example.invalid/a ");
  const beforeUncertain = refreshes;
  resolveWriter({ outcome: "unknown", message: "may already exist", items: [], refresh: false });
  await tick();
  assert.equal(refreshes, beforeUncertain);
  elements["start-clone"].click();
  assert.equal(cloneWrites.length, 2);
  statusResponse = status;
  elements.refresh.click();
  await tick();
  assert.equal(elements["setup-message"].textContent, "unknown: may already exist");
  assert.equal(elements["clone-controls"].hidden, true);

  await setupStatus("verified-clone");
  elements["clone-source"].value = "https://example.invalid/repo";
  elements["start-clone"].click();
  const beforeVerified = calls.length;
  statusResponse = status;
  resolveWriter({ outcome: "done", message: "verified", items: [], refresh: true });
  await tick();
  assert.deepEqual(calls.slice(beforeVerified), ["refresh"]);
  assert.equal(elements["setup-message"].textContent, "done: verified");
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["clone-controls"].hidden, true);
  assert.equal(cloneWrites.length, 3);
  assert.equal(calls.slice(beforeCloneCalls).filter(call => call === "run_clone_setup").length, 3);
  await setupStatus("clone-cancel");
  elements["clone-source"].value = "file:///synthetic/repo";
  elements["start-clone"].click();
  delayCancel = true;
  elements.cancel.click();
  resolveWriter({ outcome: "cancelled", message: "stopped", items: [], refresh: false });
  await tick();
  assert.match(elements["setup-message"].textContent, /waiting/);
  resolveCancel();
  delayCancel = false;
  await tick();
  assert.equal(elements["setup-message"].textContent, "stopped");
  assert.equal(elements["clone-source"].value, "");
  assert.equal(elements["clone-controls"].hidden, true);
  await setupStatus("clone-recovery");
  elements["clone-source"].value = "file:///synthetic/repo";
  elements["start-clone"].click();
  const beforeCloneRecovery = refreshes;
  resolveWriter({ outcome: "terminal_recovery", message: "terminal needed", items: [], refresh: true, recovery: [] });
  await tick();
  assert.equal(refreshes, beforeCloneRecovery);
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["clone-controls"].hidden, true);
  statusResponse = { ...status, entries: [
    { id: "ordinary", key: "ordinary", status: "changed", group: "outgoing", supported: true },
    { id: "recorded", key: "variant", status: "changed", group: "outgoing", supported: true, machine_file: { recorded: "<source>" } },
    { id: "system", key: "system", status: "changed", group: "outgoing", supported: true, system: true },
  ] };
  elements.refresh.click();
  await tick();
  const contextRows = elements.rows.children[0].children[1].children.map(item => item.children[0]);
  contextRows[1].click();
  assert.equal(elements["diff-context"].textContent, "Changed · Repository vs home · Recorded source: <source>");
  resolveDiff({ outcome: "ready", message: "ready", text: "old", file_actions: ["keep_local"] });
  await tick();
  elements["nav-review"].click();
  assert.equal(elements.diff.hidden, false);
  assert.equal(elements["diff-text"].textContent, "old");
  assert.ok(elements["diff-actions"].children.length);
  resolveReview({ id: "read-review", outcome: "terminal_recovery", message: "<review warning>", items: [] });
  await tick();
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["recovery-message"].textContent, "<review warning>");
  elements["close-review"].click();
  statusResponse = { ...status, success: false, entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, false);
  statusResponse = { ...status, entries: [
    { id: "system", key: "system", status: "changed", group: "outgoing", supported: true, system: true },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, true);
  elements.rows.children[0].children[1].children[0].children[0].click();
  assert.equal(elements["diff-context"].textContent, "Changed · Repository vs system");
  resolveDiff({ outcome: "terminal_recovery", message: "<diff warning>", text: null });
  await tick();
  assert.equal(elements["recovery-message"].textContent, "<diff warning>");
  elements["close-diff"].click();
  assert.equal(elements.recovery.hidden, false);
  statusResponse = status;
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, true);
  statusResponse = { ...status, success: false, finding: "in_progress", message: "An operation requires terminal recovery.", entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, false);
  assert.equal(elements["recovery-steps"].children.length, 0);
  assert.equal(elements["recovery-details"].hidden, true);
  assert.equal(getWorkspaceView().status, "Repository status needs attention.");
  assert.equal(elements["recovery-message"].textContent, "Repository status needs attention.");
  assert.match(elements["recovery-finding"].textContent, /operation in progress/);
  assert.equal(elements["recovery-paths"].children[3].textContent, "/r");
  const recoveryChecks = calls.length;
  elements["recovery-check"].click();
  await tick();
  assert.deepEqual(calls.slice(recoveryChecks), ["refresh"]);
  assert.equal(elements.recovery.hidden, false);
  statusResponse = status;
  elements["recovery-check"].focus();
  elements["recovery-check"].click();
  await tick();
  assert.equal(elements.recovery.hidden, true);
  assert.equal(document.activeElement, elements.message);
  statusResponse = { ...status, success: false, finding: "in_progress", message: "An operation requires terminal recovery.", entries: [] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, false);
  const beforeColdRecovery = calls.length;
  delayRefresh = true;
  elements.refresh.click();
  elements.cancel.click();
  resolveRefresh(status);
  delayRefresh = false;
  await tick();
  assert.deepEqual(calls.slice(beforeColdRecovery), ["refresh", "cancel"]);
  assert.equal(elements.recovery.hidden, false);
  statusResponse = { ...status, entries: [
    { id: "new", key: "new file", status: "new", group: "outgoing", supported: false },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements.recovery.hidden, true);
  const beforeUnsupported = diffRequests.length;
  elements.rows.children[0].children[1].children[0].children[0].click();
  assert.equal(elements["diff-context"].textContent, "New file");
  assert.equal(diffRequests.length, beforeUnsupported);
  elements["nav-review"].click();
  resolveReview({ id: "mass-review", outcome: "mass_delete", message: "approval needed", items: [] });
  await tick();
  assert.equal(elements["sync-reviewed"].hidden, true);
  assert.equal(elements["sync-mass-delete"].hidden, false);
  const beforeMass = writes.length;
  elements["sync-reviewed"].click();
  assert.equal(writes.length, beforeMass);
  elements["sync-mass-delete"].click();
  assert.equal(writes.length, beforeMass + 1);
  assert.equal(writes.at(-1).massDelete, true);
  assert.equal(writes.at(-1).selectedLines, undefined);
  resolveWriter({ outcome: "declined", message: "declined", items: [], refresh: false });
  await tick();
  assert.equal(elements["sync-mass-delete"].hidden, true);
  statusResponse = { ...status, entries: [
    { id: "variant:1", key: "file", status: "changed-locally", group: "outgoing", supported: true, system: false, host_variant: true },
    { id: "variant:2", key: "other", status: "changed-locally", group: "outgoing", supported: true, system: false },
    { id: "variant:3", key: "held", status: "held", group: "held", supported: false, system: false },
  ] };
  elements.refresh.click();
  await tick();
  assert.equal(elements["variant-result"].hidden, true);
  elements["identity-result"].hidden = true;
  elements.rows.children[0].children[1].children[0].children[0].click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "" });
  await tick();
  const variantButton = elements["diff-actions"].children.find(child => child.textContent === "Create host variant");
  assert.ok(variantButton);
  variantButton.click();
  variantButton.click();
  assert.deepEqual(variantWrites, [{ id: "variant:1" }]);
  resolveWriter({ outcome: "attention", message: "Mode settings require terminal review. No variant was created.", refresh: true });
  await tick();
  assert.equal(elements["variant-result"].hidden, false);
  assert.equal(elements["variant-result"].children[0], elements["variant-message"]);
  assert.equal(elements["variant-message"].attributes.role, "status");
  assert.equal(elements["variant-message"].attributes["aria-live"], "polite");
  assert.equal(elements["variant-message"].textContent, "Mode settings require terminal review. No variant was created.");
  elements.rows.children[0].children[1].children[1].children[0].click();
  assert.equal(elements["variant-result"].hidden, true);
  assert.equal(elements["variant-message"].textContent, "");
  resolveDiff({ success: true, outcome: "ready", message: "", text: "" });
  await tick();
  elements.rows.children[0].children[1].children[0].children[0].click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "" });
  await tick();
  elements["diff-actions"].children.find(child => child.textContent === "Create host variant").click();
  resolveWriter({ outcome: "declined", message: "Host variant was not created.", refresh: true });
  await tick();
  assert.equal(elements["variant-result"].hidden, false);
  assert.equal(elements["variant-message"].textContent, "Host variant was not created.");
  elements.rows.children[0].children[1].children[0].children[0].click();
  resolveDiff({ success: true, outcome: "ready", message: "", text: "" });
  await tick();
  elements["diff-actions"].children.find(child => child.textContent === "Create host variant").click();
  resolveWriter({ outcome: "done", message: "Host variant created.", refresh: true });
  await tick();
  assert.equal(elements["variant-result"].hidden, false);
  assert.equal(elements["variant-message"].textContent, "Host variant created.");
  const heldSelection = elements.rows.children[1].children[1].children[0].children[0];
  heldSelection.open = true;
  heldSelection.listeners.toggle();
  assert.equal(elements["variant-result"].hidden, true);
  assert.equal(elements["variant-message"].textContent, "");
  console.log("UI focus, cancellation and file decision checks passed");
}

main().catch(error => { console.error(error); process.exitCode = 1; });
