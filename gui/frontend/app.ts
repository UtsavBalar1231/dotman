import { render } from "./file-tree.js";
import { clear, show } from "./diff-reader.js";
import { runKey } from "./keys.js";
import { policyControls } from "./policy-trust.js";
import type { EntryView, FileAction, LineEdit, MachineDetails, RecoveryRow, ReviewItem, TauriCore } from "./protocol.js";

declare global {
  interface Window {
    __TAURI__: { core: TauriCore };
  }
}

const invoke = window.__TAURI__.core.invoke;
type InputId = "search" | "clone-source";
type ButtonId = "refresh" | "cancel" | "start-fresh" | "start-clone" | "start-import" | "create-age" | "import-age" | "review-identity" | "review-hooks" | "open-review" | "sync-reviewed" | "sync-mass-delete" | "sync-lines" | "leave-lines" | "close-review" | "apply-decisions" | "clear-decisions" | "next-undecided" | "close-diff" | "group-folders" | "group-status";
type ElementFor<T extends string> = T extends InputId ? HTMLInputElement
  : T extends ButtonId ? HTMLButtonElement
  : T extends "commit-message" ? HTMLTextAreaElement
  : T extends "machine-files" ? HTMLDetailsElement
  : T extends "theme-mode" | "accent-mode" ? HTMLSelectElement
  : HTMLElement;
function el<T extends string>(id: T): ElementFor<T> {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing required element: ${id}`);
  return element as ElementFor<T>;
}
let entries: EntryView[] = [];
let machineDetails: MachineDetails | null = null;
let identityReviewId: string | null = null;
let hookReviewId: string | null = null;
let setupReviewId: string | null = null;
let keyReviewId: string | null = null;
let policyReviewId: string | null = null;
let busy = false;
let serial = 0;
let cancelPending = false;
let selectedRow: HTMLElement | null = null;
let previousScroll: [number, number] = [0, 0];
let diffOrigin: { review: boolean; row: HTMLElement | undefined; id: string; scroll: [number, number]; queueScroll: number } | null = null;
const rowNodes = new Map<string, HTMLButtonElement>();
let folderMode = true;
let statusLoaded = false;
let statusLocations: { home: string; repo: string } | null = null;
let page: "files" | "review" | "settings" = "files";
let reviewId: string | null = null;
let massDeleteReview = false;
let writerActive = false;
let writerCancel: Promise<void> | null = null;
let suggestedMessage: string | null = null;
let writerKind: "files" | "identity" | "hooks" | "setup" | "sync" | "age" | "policy" | null = null;
const decisions = new Map<string, { action: FileAction; key: string }>();
let diffChoiceId: string | null = null;
let replacementReview: { entry: EntryView; token: number; launcher: HTMLButtonElement } | null = null;
const diffChoiceButtons = new Map<FileAction, HTMLButtonElement>();
let selectedOnly = false;
const selectedDefaultMessage = "sync: selected edits";
const lineReceipts = new Map<string, { path: string; edits: LineEdit[]; selected: Set<string> }>();

function clearLines() {
  if (selectedOnly && suggestedMessage !== null && el("commit-message").value === selectedDefaultMessage) {
    el("commit-message").value = suggestedMessage;
  }
  lineReceipts.clear();
  selectedOnly = false;
  renderLineSummary();
}

function renderLineSummary() {
  const root = el("selected-lines");
  root.replaceChildren();
  for (const receipt of lineReceipts.values()) {
    for (const edit of receipt.edits) {
      if (!receipt.selected.has(edit.id)) continue;
      const item = document.createElement("li");
      item.textContent = `${receipt.path} — ${lineLabel(edit)}`;
      root.append(item);
    }
  }
  el("line-summary").hidden = !selectedOnly;
  controls();
}

function lineLabel(edit: LineEdit) {
  return [edit.old_line !== null ? `Repository ${edit.old_line}: ${edit.before}` : "",
    edit.new_line !== null ? `Home ${edit.new_line}: ${edit.after}` : ""].filter(Boolean).join(" → ");
}

function validLineEdits(value: unknown): value is LineEdit[] {
  if (!Array.isArray(value) || !value.length) return false;
  const ids = new Set<string>();
  return value.every(edit => {
    if (!edit || typeof edit !== "object" || typeof edit.id !== "string" || !edit.id || ids.has(edit.id)) return false;
    ids.add(edit.id);
    const side = (number: unknown, text: unknown) => (number === null && text === null) ||
      (typeof number === "number" && Number.isSafeInteger(number) && number > 0 && typeof text === "string");
    return side(edit.old_line, edit.before) && side(edit.new_line, edit.after) &&
      (edit.old_line !== null || edit.new_line !== null);
  });
}

function renderLineEdits(entry: EntryView, review: string, token: number, edits: LineEdit[], push?: boolean) {
  const previous = lineReceipts.get(entry.id);
  const same = previous && previous.edits.length === edits.length &&
    previous.edits.every((edit, index) => edit.id === edits[index]?.id);
  const receipt = { path: entry.key, edits, selected: same ? previous.selected : new Set<string>() };
  lineReceipts.set(entry.id, receipt);
  const root = el("line-edit-list");
  root.replaceChildren();
  const other = [...lineReceipts.values()].find(candidate => candidate !== receipt && candidate.selected.size);
  if (other) {
    const help = document.createElement("p");
    help.textContent = `This review already has selected lines from ${other.path}. Finish that commit or clear its choices before choosing another file.`;
    root.append(help);
    el("line-edits").hidden = false;
    renderLineSummary();
    return;
  }
  for (const edit of edits) {
    const label = document.createElement("label");
    const input = document.createElement("input");
    input.type = "checkbox";
    input.checked = receipt.selected.has(edit.id);
    const comparison = document.createElement("span");
    comparison.className = "line-pair";
    for (const [side, number, fragment] of [
      ["Repository", edit.old_line, edit.before], ["Home", edit.new_line, edit.after],
    ] as const) {
      if (number === null || fragment === null) continue;
      const row = document.createElement("span");
      row.className = side === "Repository" ? "line-before" : "line-after";
      const location = document.createElement("span");
      location.textContent = `${side === "Repository" ? "−" : "+"} ${side} ${number}`;
      const code = document.createElement("code");
      code.textContent = fragment;
      row.append(location, code);
      comparison.append(row);
    }
    label.append(input, comparison);
    input.addEventListener("change", () => {
      if (busy || cancelPending || token !== serial || reviewId !== review || diffOrigin?.id !== entry.id || !diffOrigin.review || lineReceipts.get(entry.id) !== receipt) return;
      if (!selectedOnly && suggestedMessage !== null && el("commit-message").value === suggestedMessage) {
        el("commit-message").value = selectedDefaultMessage;
      }
      selectedOnly = true;
      el("selected-effect").textContent = push === true
        ? "Commit only these edits, then attempt a non-forced push. A rejected push leaves the commit local. No pull, apply, or hooks."
        : push === false
        ? "Commit only these edits locally. No remote is configured. The other edits stay in Home."
        : "Commit only these edits. Check the target in the native confirmation.";
      el("sync-lines").textContent = push === true ? "Commit selected lines and push" : "Commit selected lines";
      if (input.checked) receipt.selected.add(edit.id);
      else receipt.selected.delete(edit.id);
      renderLineSummary();
    });
    root.append(label);
  }
  el("line-edits").hidden = false;
  renderLineSummary();
}
const actionLabels = new Map<FileAction, string>([
  ["keep_local", "Keep local"], ["take_repo", "Take repo (backs up replaced home bytes)"], ["restore", "Restore"],
]);

function renderDecisions() {
  const root = el("decision-items");
  root.replaceChildren();
  for (const [id, choice] of decisions) {
    const item = document.createElement("li");
    const path = document.createElement("span");
    path.className = "decision-key";
    path.textContent = choice.key;
    const label = document.createElement("span");
    label.textContent = ` — ${actionLabels.get(choice.action)}`;
    const remove = document.createElement("button");
    remove.textContent = "Undo";
    remove.setAttribute("aria-label", `Undo decision: ${choice.key}`);
    remove.addEventListener("click", () => {
      if (busy || cancelPending) return;
      decisions.delete(id);
      renderDecisions();
    });
    item.append(path, label, remove);
    root.append(item);
  }
  const selected = diffChoiceId === null ? undefined : decisions.get(diffChoiceId)?.action;
  for (const [action, button] of diffChoiceButtons) {
    button.setAttribute("aria-pressed", action === selected ? "true" : "false");
  }
  el("diff-selection").textContent = diffChoiceButtons.size
    ? (selected ? `Selected: ${actionLabels.get(selected)}` : "No choice selected.")
    : "";
  controls();
}

function clearDecisions() {
  decisions.clear();
  renderDecisions();
}

function clearCommitEditor() {
  suggestedMessage = null;
  el("commit-message").value = "";
  el("commit-editor").hidden = true;
  el("commit-error").textContent = "";
  el("commit-message").setAttribute("aria-invalid", "false");
}

function commitError() {
  if (suggestedMessage === null || el("commit-message").value === suggestedMessage) return "";
  const value = el("commit-message").value;
  if (!value.trim()) return "Enter a nonblank message, or restore the suggestion.";
  if (new TextEncoder().encode(value).length > 1024) return "Use at most 1024 UTF-8 bytes.";
  if (value.split("\n").length > 6) return "Use at most six lines.";
  if (/[\p{Cc}\p{Cs}\u00ad\u061c\u180e\u200b\u200e\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\ufff9-\ufffb\u{e0001}\u{e0020}-\u{e007f}]/u.test(value.replaceAll("\n", ""))) {
    return "Remove control or invisible formatting characters.";
  }
  return "";
}

function updateCommitValidation() {
  const error = commitError();
  el("commit-error").textContent = error;
  el("commit-message").setAttribute("aria-invalid", error ? "true" : "false");
  controls();
}

function controls() {
  for (const name of ["files", "review", "settings"] as const) {
    (el(`nav-${name}`) as HTMLButtonElement).disabled = busy || cancelPending;
  }
  el("paths-unavailable").hidden = !el("context").hidden;
  el("age-unavailable").hidden = keyReviewId !== null;
  el("age-unavailable").textContent = statusLoaded
    ? "Age identity controls are not available for this status."
    : "Age identity controls are unavailable until status is loaded.";
  (el("policy-directories") as HTMLButtonElement).disabled = busy || cancelPending || !statusLoaded || policyReviewId === null;
  (el("policy-ignores") as HTMLButtonElement).disabled = busy || cancelPending || !statusLoaded || policyReviewId === null;
  el("age-controls").hidden = keyReviewId === null;
  el("create-age").disabled = busy || cancelPending;
  el("import-age").disabled = busy || cancelPending;
  el("next-undecided").hidden = !statusLoaded || el("diff").hidden || !diffOrigin || diffOrigin.review || !nextUndecided();
  el("next-undecided").disabled = busy || cancelPending;
  el("decisions").hidden = !decisions.size && !el("decision-message").textContent && !(writerActive && writerKind === "files");
  el("refresh").disabled = busy || cancelPending;
  (el("recovery-check") as HTMLButtonElement).disabled = busy || cancelPending;
  (el("setup-check") as HTMLButtonElement).disabled = busy || cancelPending;
  el("cancel").hidden = !busy && !cancelPending;
  el("cancel").disabled = !busy || cancelPending;
  el("cancel").textContent = writerActive ? (writerKind === "files" ? "Cancel apply" : writerKind === "identity" ? "Cancel identity review" : writerKind === "hooks" ? "Cancel hook review" : writerKind === "setup" ? "Cancel setup" : writerKind === "age" ? "Cancel identity operation" : writerKind === "policy" ? "Cancel policy review" : "Cancel sync") : "Cancel read";
  el("start-fresh").hidden = setupReviewId === null;
  el("start-fresh").disabled = busy || cancelPending;
  el("start-import").hidden = setupReviewId === null;
  el("start-import").disabled = busy || cancelPending;
  el("clone-controls").hidden = setupReviewId === null;
  el("clone-source").disabled = busy || cancelPending;
  el("start-clone").disabled = busy || cancelPending || !el("clone-source").value;
  el("review-identity").hidden = identityReviewId === null;
  el("review-identity").disabled = busy || cancelPending || !statusLoaded;
  el("review-hooks").hidden = hookReviewId === null;
  el("review-hooks").disabled = busy || cancelPending || !statusLoaded;
  el("open-review").disabled = busy || cancelPending;
  el("sync-reviewed").hidden = reviewId === null || selectedOnly || massDeleteReview;
  el("sync-reviewed").disabled = busy || cancelPending || !!commitError();
  el("sync-mass-delete").hidden = reviewId === null || !massDeleteReview;
  el("sync-mass-delete").disabled = busy || cancelPending || !!commitError();
  el("sync-lines").hidden = reviewId === null || !selectedOnly;
  el("sync-lines").disabled = busy || cancelPending || !!commitError() || !Array.from(lineReceipts.values()).some(receipt => receipt.selected.size);
  el("leave-lines").disabled = busy || cancelPending;
  el("close-review").disabled = writerActive;
  el("apply-decisions").disabled = busy || cancelPending || !statusLoaded || !decisions.size;
  el("clear-decisions").disabled = busy || cancelPending || !decisions.size;
}

function clearDiff() {
  el("diff").hidden = true;
  el("diff-key").textContent = "";
  el("diff-context").textContent = "";
  el("diff-message").textContent = "";
  clear();
  el("diff-actions").replaceChildren();
  el("diff-selection").textContent = "";
  el("line-edits").hidden = true;
  el("line-edit-list").replaceChildren();
  diffChoiceId = null;
  diffChoiceButtons.clear();
  replacementReview = null;
  el("replacement-review").hidden = true;
}

function clearVariantResult() {
  el("variant-result").hidden = true;
  el("variant-message").textContent = "";
}

function safeDisplayPath(value: unknown): value is string {
  return typeof value === "string" && value.length > 0 &&
    !/[\p{Cc}\p{Cs}\u00ad\u061c\u180e\u200b\u200e\u200f\u2028-\u202e\u2060-\u2064\u2066-\u206f\ufeff\ufff9-\ufffb\u{e0001}\u{e0020}-\u{e007f}]/u.test(value);
}

const groups = [
  ["decision", "Needs a decision"], ["outgoing", "Local changes and new files"],
  ["incoming", "Changes in the repository"], ["held", "Held back"],
  ["unavailable", "Cannot be compared or applied"], ["other", "Other status"],
];

const holdOrigins = new Map([
  ["new_file", "New file"], ["home_edit", "Home edit"], ["repo_edit", "Repository edit"],
]);

function renderHeldCounts(counts: { new_files: number; home_edits: number; repo_edits: number } | undefined) {
  const count = (key: "new_files" | "home_edits" | "repo_edits") => {
    const value = counts?.[key];
    return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
  };
  const values = [count("new_files"), count("home_edits"), count("repo_edits")];
  el("held-counts").hidden = !values.some(value => value !== null && value > 0);
  el("held-counts").textContent = `Held in full report: new files ${values[0] ?? "unknown"} · home edits ${values[1] ?? "unknown"} · repository edits ${values[2] ?? "unknown"}`;
}

function heldRow(entry: EntryView) {
  const details = document.createElement("details");
  details.className = "held-row";
  const summary = document.createElement("summary");
  summary.className = "row";
  const key = document.createElement("span");
  key.className = "row-key";
  key.textContent = entry.key;
  const status = document.createElement("span");
  status.className = "row-status";
  status.textContent = `${entry.status} · details`;
  summary.append(key, status);
  details.append(summary);
  details.addEventListener("toggle", () => {
    if (details.open) clearVariantResult();
    if (!details.open || details.children.length > 1) return;
    const hold = entry.hold;
    const fields = [
      ["Origin", hold?.origin ? holdOrigins.get(hold.origin) ?? "Unknown" : "Unknown"],
      ["Reason", typeof hold?.reason === "string" ? hold.reason : "Unavailable"],
      ["Rule", hold?.rule],
      ["Location", hold?.location],
      ["Terminal guidance", hold?.guidance],
    ];
    for (const [label, value] of fields) {
      if (typeof value !== "string" || !value) continue;
      const line = document.createElement("p");
      line.textContent = `${label}: ${value}`;
      details.append(line);
    }
    if (entry.held_actions?.includes("add")) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = "Review add";
      button.addEventListener("click", () => runHeld(entry.id, "add"));
      details.append(button);
    }
    if (entry.held_actions?.includes("plain")) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = "Store unencrypted";
      button.addEventListener("click", () => runHeld(entry.id, "plain"));
      details.append(button);
    }
    if (entry.held_actions?.includes("encrypt")) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = "Encrypt";
      button.addEventListener("click", () => runHeld(entry.id, "encrypt"));
      details.append(button);
    }
    if (entry.held_actions?.includes("forget")) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = "Forget";
      button.addEventListener("click", () => runHeld(entry.id, "forget"));
      details.append(button);
    }
  });
  return details;
}

async function runHeld(id: string, action: "add" | "plain" | "encrypt" | "forget") {
  if (busy || cancelPending || !statusLoaded || !el("recovery").hidden) return;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "files";
  writerCancel = null;
  busy = true;
  statusLoaded = false;
  el("identity-result").hidden = false;
  el("identity-message").textContent = "Waiting for native held-file review…";
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("run_held", { id, action });
    if (writerCancel) await writerCancel;
    el("identity-message").textContent = `${result.outcome}: ${result.message}`;
    shouldRefresh = result.refresh === true;
  } catch {
    el("identity-message").textContent = "The held-file action could not be verified; check status before trying again.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

function fileLeaf(entry: EntryView, displayName: string) {
  const item = document.createElement("li");
  if (entry.group === "held") {
    item.append(heldRow(entry));
    return item;
  }
  const row = document.createElement("button");
  row.type = "button";
  row.className = "row";
  row.setAttribute("data-group", entry.group);
  row.setAttribute("aria-current", diffOrigin?.id === entry.id ? "true" : "false");
  rowNodes.set(entry.id, row);
  const key = document.createElement("span");
  key.className = "row-key";
  key.textContent = displayName;
  const status = document.createElement("span");
  status.className = "row-status";
  if (entry.status === "changed-in-repo" && !entry.system && entry.supported) {
    status.className += " repository-change";
    status.textContent = "Repository changed";
  } else {
    status.textContent = `${entry.status}${entry.system ? " · system (read only)" : ""}${entry.supported ? "" : " · diff unavailable"}`;
  }
  row.setAttribute("aria-label", `${entry.key} — ${status.textContent}`);
  row.title = entry.key;
  row.append(key, status);
  row.addEventListener("click", () => openDiff(entry, row));
  item.append(row);
  return item;
}

async function runHostVariant(id: string) {
  if (busy || cancelPending || !statusLoaded || diffOrigin?.id !== id) return;
  busy = true;
  writerActive = true;
  writerKind = "files";
  controls();
  el("variant-result").hidden = false;
  el("variant-message").textContent = "Waiting for native host-variant review…";
  let shouldRefresh = false;
  try {
    const result = await invoke("run_host_variant", { id });
    el("variant-message").textContent = result.message;
    shouldRefresh = result.refresh === true;
  } catch {
    el("variant-message").textContent = "The variant could not be verified; check status before trying again.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh(false, true);
}

function renderRows() {
  const root = el("rows");
  root.replaceChildren();
  rowNodes.clear();
  const query = el("search").value.toLocaleLowerCase();
  if (folderMode) {
    if (!render(root, entries, query, fileLeaf)) {
      root.textContent = query ? "No files match this search." : "No files need attention.";
    }
    return;
  }
  let visible = 0;
  for (const [group, label] of groups) {
    const found = entries.filter(entry => entry.group === group && (entry.key.toLocaleLowerCase().includes(query) || entry.status.toLocaleLowerCase().includes(query)));
    if (!found.length) continue;
    visible += found.length;
    const section = document.createElement("section");
    const heading = document.createElement("h3");
    heading.textContent = `${label} (${found.length})`;
    const list = document.createElement("ul");
    for (const entry of found) list.append(fileLeaf(entry, entry.key));
    section.append(heading, list);
    root.append(section);
  }
  if (!visible) {
    const empty = document.createElement("p");
    empty.textContent = query ? "No files match this search." : "No files need attention.";
    root.append(empty);
  }
}

function nextUndecided() {
  if (!entries.length || !diffOrigin) return null;
  const id = diffOrigin.id;
  const start = entries.findIndex(entry => entry.id === id);
  for (let step = 1; step < entries.length; step++) {
    const entry = entries[(start + step) % entries.length];
    if (entry.group === "decision" && !entry.system && entry.supported && !decisions.has(entry.id)) return entry;
  }
  return null;
}

function renderWorklist() {
  const root = el("attention-list");
  root.replaceChildren();
  const inspect = entries.filter(entry => entry.group === "decision" && entry.supported && !entry.system);
  const changes = entries.filter(entry => entry.supported && !entry.system && ["outgoing", "incoming"].includes(entry.group));
  const list = inspect.length ? inspect : changes;
  el("guide-title").textContent = inspect.length ? "Needs a decision" : changes.length ? "Changes to inspect" : "File comparison";
  el("guide-description").textContent = inspect.length ? `${inspect.length} to inspect · Compare these files before choosing how to resolve them.` :
    changes.length ? "Select a file to compare its copies." :
    entries.length || !el("held-counts").hidden ? "No comparable changes are available here. Check held counts and the file list for details." : "No files need attention.";
  for (const entry of list.slice(0, 6)) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    for (const text of [entry.key, inspect.length ? entry.status.replaceAll("-", " ") : entry.group === "outgoing" ? "Local change" : "Repository change"]) {
      const span = document.createElement("span");
      span.textContent = text;
      button.append(span);
    }
    button.addEventListener("click", () => openDiff(entry, button));
    item.append(button);
    root.append(item);
  }
}

function clearMachineFiles() {
  machineDetails = null;
  identityReviewId = null;
  hookReviewId = null;
  setupReviewId = null;
  policyReviewId = null;
  keyReviewId = null;
  el("start-fresh").hidden = true;
  el("start-import").hidden = true;
  el("clone-controls").hidden = true;
  el("clone-source").value = "";
  el("review-identity").hidden = true;
  el("machine-files").hidden = true;
  el("machine-file-details").replaceChildren();
}

function renderMachineFiles() {
  const root = el("machine-file-details");
  root.replaceChildren();
  if (!statusLoaded || el("machine-files").hidden || !el("machine-files").open) return;
  if (machineDetails) {
    const list = document.createElement("dl");
    for (const [label, value] of [
      ["Host", machineDetails.host], ["OS", machineDetails.os], ["Architecture", machineDetails.arch],
      ["Profiles", Array.isArray(machineDetails.profiles) ? machineDetails.profiles.join(", ") : null],
    ]) {
      if (typeof value !== "string" || !value) continue;
      const term = document.createElement("dt");
      const description = document.createElement("dd");
      term.textContent = label;
      description.textContent = value;
      list.append(term, description);
    }
    root.append(list);
  }
  const list = document.createElement("ul");
  for (const entry of entries) {
    const file = entry.machine_file;
    if (!file) continue;
    const item = document.createElement("li");
    const key = document.createElement("div");
    key.className = "machine-path";
    key.textContent = entry.key;
    item.append(key);
    for (const [label, value] of [
      ["Selected source", file.selected], ["Recorded source", file.recorded],
      ["Other reported sources", Array.isArray(file.alternatives) ? file.alternatives : []],
    ]) {
      for (const name of Array.isArray(value) ? value : [value]) {
        if (typeof name !== "string" || !name) continue;
        const line = document.createElement("p");
        const path = document.createElement("span");
        path.className = "machine-path";
        path.textContent = name;
        line.textContent = `${label}: `;
        line.append(path);
        item.append(line);
      }
    }
    const flags = document.createElement("p");
    flags.textContent = `Kind: ${file.kind} · Ambiguous: ${file.ambiguous ? "yes" : "no"} · Identity changed: ${file.identity_changed ? "yes" : "no"}`;
    item.append(flags);
    list.append(item);
  }
  root.append(list);
}

async function refresh(preserveReview = false, preserveVariantResult = false) {
  if (busy || cancelPending) return;
  const setupWasVisible = !el("setup").hidden;
  if (!preserveVariantResult) clearVariantResult();
  clearLines();
  clearDecisions();
  clearCommitEditor();
  const token = ++serial;
  busy = true;
  controls();
  clearDiff();
  diffOrigin = null;
  if (!preserveReview) {
    el("review").hidden = true;
    el("review-items").replaceChildren();
  }
  reviewId = null;
  entries = [];
  statusLoaded = false;
  statusLocations = null;
  clearMachineFiles();
  el("setup").hidden = true;
  el("entries").hidden = true;
  el("context").hidden = true;
  el("paths").replaceChildren();
  el("repository-path").textContent = "";
  el("message").textContent = "Loading status…";
  try {
    const view = await invoke("refresh");
    if (token !== serial) return;
    if (view.success) {
      el("recovery").hidden = true;
      el("recovery-message").textContent = "";
      el("recovery-steps").replaceChildren();
      el("recovery-details").hidden = true;
    } else if (["in_progress", "pull_conflict", "stopped"].includes(view.finding)) {
      showRecovery("Repository status needs attention.");
    }
    el("notice").hidden = !view.key_stripped;
    el("message").textContent = !view.success && ["in_progress", "pull_conflict", "stopped"].includes(view.finding)
      ? "Repository status needs attention."
      : view.message;
    if (view.context) {
      el("repository-path").textContent = view.context.repo;
      for (const [label, key] of [["Home", "home"], ["Repository", "repo"], ["Configuration", "config"], ["State", "state"], ["Identity", "identity"], ["Machine", "machine"]] as const) {
        const term = document.createElement("dt");
        const value = document.createElement("dd");
        term.textContent = label;
        value.textContent = view.context[key];
        el("paths").append(term, value);
      }
      el("context").hidden = false;
    }
    for (const id of ["recovery-paths", "setup-paths"]) {
      const root = el(id);
      root.replaceChildren();
      if (view.context) {
        for (const [label, value] of [["Home", view.context.home], ["Repository", view.context.repo], ["Configuration", view.context.config], ["State", view.context.state]]) {
          const term = document.createElement("dt");
          const description = document.createElement("dd");
          term.textContent = label;
          description.textContent = value;
          root.append(term, description);
        }
      }
    }
    el("recovery-finding").textContent = view.finding === "in_progress"
      ? "Dotman reports an operation in progress."
      : view.finding === "pull_conflict"
        ? "Dotman reports a pull conflict."
        : "Dotman reports a stopped operation; its cause is not available here.";
    entries = view.entries;
    statusLoaded = view.success;
    statusLocations = view.success && view.context ? { home: view.context.home, repo: view.context.repo } : null;
    machineDetails = view.success ? view.context?.machine_details ?? null : null;
    identityReviewId = view.success && typeof view.identity_review_id === "string" && view.identity_review_id.length ? view.identity_review_id : null;
    policyReviewId = view.success && typeof view.policy_review_id === "string" && view.policy_review_id.length ? view.policy_review_id : null;
    keyReviewId = view.success && typeof view.key_review_id === "string" && view.key_review_id.length ? view.key_review_id : null;
    hookReviewId = view.success && typeof view.hook_review_id === "string" && view.hook_review_id.length ? view.hook_review_id : null;
    el("setup").hidden = !(!view.success && view.repo_unavailable === true && view.context && !preserveReview && el("recovery").hidden);
    setupReviewId = !el("setup").hidden && view.repo_unavailable === true && typeof view.context?.repo === "string" && view.context.repo.length &&
      typeof view.setup_review_id === "string" && view.setup_review_id.length ? view.setup_review_id : null;
    el("setup-finding").textContent = view.finding === "unavailable"
      ? "Dotman could not open a repository at the resolved location."
      : "The repository status could not be verified.";
    el("setup-guidance").textContent = setupReviewId === null
      ? "Setup is not available for this status. An existing repository may need attention outside this panel."
      : "Choose Create, Clone, or Import to review a setup. No files are changed until you confirm.";
    el("machine-files").hidden = !view.success || (identityReviewId === null && !entries.some(entry => entry.machine_file));
    if (!el("machine-files").hidden) renderMachineFiles();
    el("remote").textContent = view.remote;
    el("clean").textContent = `${view.clean} clean`;
    renderHeldCounts(view.held_counts);
    renderRows();
    renderWorklist();
    el("entries").hidden = preserveReview || !view.success;
  } catch {
    if (token === serial) {
      el("message").textContent = "Status read failed.";
      if (setupWasVisible && el("recovery").hidden) {
        el("setup").hidden = false;
        el("setup-finding").textContent = "The repository status could not be verified.";
        el("setup-guidance").textContent = "Setup is not available until a successful check permits it.";
        el("setup-paths").replaceChildren();
      }
    }
  } finally {
    busy = false;
    controls();
  }
}

async function openDiff(entry: EntryView, row: HTMLElement | undefined, review: string | null = null) {
  if (busy || cancelPending) return;
  if (review !== null && (reviewId !== review || el("review").hidden)) return;
  if (review === null && (!statusLoaded || !entries.includes(entry))) return;
  clearVariantResult();
  if (diffOrigin && !diffOrigin.review) rowNodes.get(diffOrigin.id)?.setAttribute("aria-current", "false");
  diffOrigin = { review: review !== null, row, id: entry.id, scroll: [window.scrollX, window.scrollY], queueScroll: el("rows").scrollTop };
  clearDiff();
  if (review === null) rowNodes.get(entry.id)?.setAttribute("aria-current", "true");
  if (diffOrigin.review) el("review").hidden = true;
  el("diff").hidden = false;
  el("diff-key").textContent = entry.key;
  el("diff-context").textContent = review !== null ? "Capture preview" : entry.status;
  if (review === null && entry.host_variant) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Create host variant";
    button.addEventListener("click", () => runHostVariant(entry.id));
    el("diff-actions").append(button);
  }
  if (review === null && typeof entry.edit_command === "string") {
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = "Source editing details";
    const label = document.createElement("label");
    label.textContent = entry.status === "template-drift"
      ? "Edit the template in a terminal:"
      : "Edit the managed source in a terminal:";
    const command = document.createElement("textarea");
    command.readOnly = true;
    command.rows = 2;
    command.setAttribute("aria-label", "Terminal edit command");
    command.value = entry.edit_command;
    label.append(command);
    details.append(summary, label);
    el("diff-actions").append(details);
  }
  el("close-diff").focus();
  if (!entry.supported) {
    if (review === null) {
      decisions.delete(entry.id);
      renderDecisions();
    }
    el("diff-message").textContent = "Diff is unavailable for this occurrence.";
    return;
  }
  if (review === null) {
    const recorded = entry.machine_file?.recorded;
    if (entry.status === "changed-in-repo" && !entry.system) {
      el("diff-context").textContent = "The repository copy has changed. Compare it with the file on this computer.";
    } else el("diff-context").textContent += ` · Repository vs ${entry.system ? "system" : "home"}`;
    if (typeof recorded === "string" && recorded) el("diff-context").textContent += ` · Recorded source: ${recorded}`;
  }
  const token = ++serial;
  busy = true;
  controls();
  el("diff-message").textContent = "Loading diff…";
  try {
    const view = await invoke("open_diff", { id: entry.id });
    if (token !== serial) return;
    if (review !== null) {
      if (view.outcome === "ready" && view.success && validLineEdits(view.line_edits)) {
        renderLineEdits(entry, review, token, view.line_edits, view.selected_push);
      } else {
        lineReceipts.delete(entry.id);
        renderLineSummary();
      }
    }
    el("diff-message").textContent = view.outcome === "ready" && view.message === "different" ? "" : view.message;
    if (review === null && entry.status === "changed-in-repo" && entry.group === "incoming" &&
        !entry.system && view.outcome === "ready" && !view.file_actions?.includes("take_repo")) {
      el("diff-message").textContent = [el("diff-message").textContent, "Replacement review is not available for this file."].filter(Boolean).join(" ");
    }
    if (view.outcome === "ready" && typeof view.text === "string") {
      show(view.text, entry.system ? "System" : "Home");
    } else {
      el("diff-text").textContent = view.text ?? "No diff payload.";
    }
    const source = entry.machine_file ? entry.machine_file.selected : entry.key;
    const actions = review === null && !entry.system && !["held", "unavailable", "other"].includes(entry.group) &&
      (entry.status !== "changed-in-repo" || (entry.group === "incoming" && view.success === true &&
        safeDisplayPath(statusLocations?.repo) && safeDisplayPath(statusLocations?.home) &&
        safeDisplayPath(entry.key) && safeDisplayPath(source))) &&
      view.outcome === "ready" && Array.isArray(view.file_actions)
      ? view.file_actions.filter((action): action is FileAction =>
        entry.status === "changed-in-repo" ? action === "take_repo" :
          action === "keep_local" || action === "take_repo" || action === "restore") : [];
    if (review === null) {
      const choice = decisions.get(entry.id);
      if (choice && !actions.includes(choice.action)) decisions.delete(entry.id);
    }
    if (actions.length) {
      diffChoiceId = entry.id;
      for (const action of new Set(actions)) {
        const button = document.createElement("button");
        const replacement = entry.status === "changed-in-repo" && entry.group === "incoming" &&
          !entry.system && view.success === true && action === "take_repo" && statusLocations !== null;
        button.textContent = replacement ? "Review replacement" : actionLabels.get(action) ?? "";
        if (replacement) {
          button.className = "review-action";
          const arrow = document.createElementNS("http://www.w3.org/2000/svg", "svg");
          arrow.setAttribute("viewBox", "0 0 24 24");
          arrow.setAttribute("aria-hidden", "true");
          arrow.setAttribute("focusable", "false");
          const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
          path.setAttribute("d", "M4 12h15m-6-6 6 6-6 6");
          arrow.append(path);
          button.append(arrow);
        }
        button.addEventListener("click", () => {
          if (busy || cancelPending || token !== serial || !statusLoaded || !entries.includes(entry)) return;
          if (replacement) {
            if (replacementReview) return;
            if (!statusLocations || !safeDisplayPath(source)) return;
            replacementReview = { entry, token, launcher: button };
            el("replacement-source").textContent = `${statusLocations.repo.replace(/\/$/, "")}/${source}`;
            el("replacement-target").textContent = `${statusLocations.home.replace(/\/$/, "")}/${entry.key}`;
            el("replacement-review").hidden = false;
            el("replacement-title").focus();
            return;
          }
          decisions.set(entry.id, { action, key: entry.key });
          renderDecisions();
        });
        if (!replacement) diffChoiceButtons.set(action, button);
        el("diff-actions").append(button);
      }
      if (entry.status !== "changed-in-repo") {
        const skip = document.createElement("button");
        skip.textContent = "Skip / clear choice";
        skip.addEventListener("click", () => {
          if (busy || cancelPending || token !== serial || !statusLoaded || !entries.includes(entry)) return;
          decisions.delete(entry.id);
          renderDecisions();
        });
        el("diff-actions").append(skip);
      }
    }
    if (review === null) renderDecisions();
    if (view.outcome === "terminal_recovery") {
      showRecovery(view.message);
      clearDecisions();
      entries = [];
      statusLoaded = false;
      clearMachineFiles();
      el("entries").hidden = true;
      el("message").textContent = view.message;
      if (diffOrigin?.review) {
        reviewId = null;
        clearCommitEditor();
        el("review-items").replaceChildren();
      }
    }
  } catch {
    if (token === serial) {
      if (review !== null) {
        lineReceipts.delete(entry.id);
        renderLineSummary();
      }
      if (review === null) {
        decisions.delete(entry.id);
        renderDecisions();
      }
      el("diff-message").textContent = "Diff read failed.";
    }
  } finally {
    busy = false;
    controls();
  }
}

async function cancel() {
  if (!busy || cancelPending) return;
  clearLines();
  clearDecisions();
  if (writerActive) {
    cancelPending = true;
    const target = writerKind === "files" ? "decision-message" : writerKind === "identity" ? "identity-message" : writerKind === "hooks" ? "hooks-message" : writerKind === "setup" ? "setup-message" : writerKind === "policy" ? "policy-message" : "review-message";
    const operation = writerKind === "files" ? "apply" : writerKind === "identity" ? "identity review" : writerKind === "hooks" ? "hook review" : writerKind === "setup" ? "setup" : writerKind === "policy" ? "policy review" : "sync";
    el(target).textContent = `Cancelling ${operation}; waiting for the result…`;
    controls();
    writerCancel = invoke("cancel").catch(() => {
      el(target).textContent = `Could not request cancellation; waiting for the ${operation} result…`;
    });
    await writerCancel;
    cancelPending = false;
    controls();
    return;
  }
  ++serial;
  cancelPending = true;
  clearDiff();
  diffOrigin = null;
  el("review").hidden = true;
  el("review-items").replaceChildren();
  clearCommitEditor();
  reviewId = null;
  setPage("files");
  controls();
  selectedRow = null;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  el("setup").hidden = true;
  el("entries").hidden = true;
  el("message").textContent = "Cancelling read…";
  el("message").focus();
  controls();
  try {
    await invoke("cancel");
    el("message").textContent = "Read cancelled. Refresh status to continue.";
  } catch {
    el("message").textContent = "Could not cancel read.";
  } finally {
    cancelPending = false;
    controls();
  }
}

function closeDiff() {
  if (busy) {
    void cancel();
    return;
  }
  ++serial;
  clearDiff();
  if (diffOrigin?.review && reviewId !== null) {
    el("review").hidden = false;
    if (diffOrigin.row?.isConnected) diffOrigin.row.focus({ preventScroll: true });
    window.scrollTo(...diffOrigin.scroll);
    diffOrigin = null;
    return;
  }
  el("context").hidden = !el("paths").children.length;
  if (statusLoaded) {
    el("entries").hidden = false;
    if (diffOrigin) {
      el("rows").scrollTop = diffOrigin.queueScroll;
      window.scrollTo(...diffOrigin.scroll);
    }
    const row = diffOrigin ? rowNodes.get(diffOrigin.id) : undefined;
    if (row) {
      for (let parent = row.parentElement; parent && parent !== el("rows"); parent = parent.parentElement) {
        if (parent instanceof HTMLDetailsElement) parent.open = true;
      }
      row.setAttribute("aria-current", "false");
      row.focus({ preventScroll: true });
      row.scrollIntoView({ block: "nearest", inline: "nearest" });
    } else {
      el("search").focus();
    }
  } else {
    el("message").focus();
  }
  selectedRow = null;
  diffOrigin = null;
}

async function openReview() {
  if (busy || cancelPending || !statusLoaded) return;
  clearLines();
  selectedRow = el("open-review");
  previousScroll = [window.scrollX, window.scrollY];
  if (diffOrigin && !diffOrigin.review) {
    rowNodes.get(diffOrigin.id)?.setAttribute("aria-current", "false");
    clearDiff();
    diffOrigin = null;
  }
  el("entries").hidden = true;
  el("review").hidden = false;
  el("review-items").replaceChildren();
  clearCommitEditor();
  reviewId = null;
  controls();
  el("review-remote").textContent = el("remote").textContent;
  el("review-message").textContent = "Loading outgoing preview…";
  el("close-review").focus();
  const token = ++serial;
  busy = true;
  controls();
  try {
    const view = await invoke("open_review");
    if (token !== serial) return;
    reviewId = view.id ?? null;
    massDeleteReview = view.outcome === "mass_delete";
    if (view.outcome === "review" && typeof reviewId === "string" && reviewId.length && typeof view.commit_message === "string" && view.commit_message.length) {
      suggestedMessage = view.commit_message;
      el("commit-message").value = suggestedMessage;
      el("commit-editor").hidden = false;
    }
    el("review-message").textContent = view.message;
    if (view.outcome === "terminal_recovery") {
      showRecovery(view.message);
      clearDecisions();
      entries = [];
      statusLoaded = false;
      clearMachineFiles();
      el("message").textContent = view.message;
    }
    renderReviewItems(view.items, view.outcome === "review" && typeof reviewId === "string" && reviewId.length > 0);
  } catch {
    if (token === serial) el("review-message").textContent = "Sync preview could not be loaded.";
  } finally {
    busy = false;
    controls();
  }
}

function renderReviewItems(items: ReviewItem[], captureDiffs = false) {
  el("review-items").replaceChildren();
  const groups = new Map<string, ReviewItem[]>();
  for (const item of items) {
    if (!groups.has(item.group)) groups.set(item.group, []);
    groups.get(item.group)?.push(item);
  }
  for (const [label, items] of groups) {
    const heading = document.createElement("h3");
    const list = document.createElement("ul");
    heading.textContent = `${label} (${items.length})`;
    for (const item of items) {
      const row = document.createElement("li");
      row.textContent = item.text;
      if (captureDiffs && typeof item.diff_id === "string" && item.diff_id.length) {
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = "View capture diff";
        button.setAttribute("aria-label", `View capture diff: ${item.text}`);
        const id = reviewId;
        const diffId = item.diff_id;
        button.addEventListener("click", () => openDiff({ id: diffId, key: item.text, status: "", group: "", system: false, supported: true }, button, id));
        row.append(button);
      }
      list.append(row);
    }
    el("review-items").append(heading, list);
  }
}

function showRecovery(message: string, steps?: RecoveryRow[]) {
  clearLines();
  el("recovery-message").textContent = message;
  const root = el("recovery-steps");
  if (Array.isArray(steps)) {
    root.replaceChildren();
    for (const [index, step] of steps.entries()) {
      if (step.kind === "instruction") {
        const instruction = document.createElement("p");
        instruction.textContent = step.text;
        root.append(instruction);
      } else if (step.kind === "command") {
        const block = document.createElement("div");
        if (step.condition !== null) {
          const condition = document.createElement("p");
          condition.textContent = step.condition;
          block.append(condition);
        }
        const command = document.createElement("pre");
        command.tabIndex = 0;
        command.setAttribute("aria-label", `Recovery command ${index + 1}`);
        command.textContent = step.shell;
        const copy = document.createElement("button");
        copy.type = "button";
        copy.textContent = `Copy command ${index + 1}`;
        const feedback = document.createElement("p");
        feedback.setAttribute("role", "status");
        feedback.setAttribute("aria-live", "polite");
        copy.addEventListener("click", async () => {
          try {
            await window.navigator.clipboard.writeText(step.shell);
            feedback.textContent = "Command copied.";
          } catch {
            feedback.textContent = "Could not copy. Select the command and copy it manually.";
          }
        });
        block.append(command, copy, feedback);
        root.append(block);
      }
    }
  }
  el("recovery-details").hidden = root.children.length === 0;
  el("recovery").hidden = false;
}

const syncStages = new Map([
  ["started", "Sync process started."],
  ["capture", "Sync stage reached: capturing changes."],
  ["commit", "Sync stage reached: committing changes."],
  ["pull", "Sync stage reached: pulling changes."],
  ["apply", "Sync stage reached: applying changes."],
  ["reencrypt", "Sync stage reached: reencrypting secrets."],
  ["hooks", "Sync stage reached: checking the no-hooks policy."],
  ["push", "Sync stage reached: pushing changes."],
  ["blocked_by_repo_hold", "Sync stage reached: blocked by a repository hold."],
]);

async function syncReviewed(selected = false, massDelete = false) {
  if (busy || cancelPending || reviewId === null || commitError()) return;
  if (massDelete !== massDeleteReview || (massDelete && selected)) return;
  if (selected !== selectedOnly) return;
  const selectedLines = Array.from(lineReceipts.values()).flatMap(receipt => Array.from(receipt.selected));
  if (selected && !selectedLines.length) return;
  const id = reviewId;
  const draft = el("commit-message").value;
  const defaultMessage = selected ? selectedDefaultMessage : suggestedMessage;
  const commitMessage = suggestedMessage !== null && draft !== defaultMessage ? draft : null;
  clearCommitEditor();
  clearDecisions();
  clearLines();
  reviewId = null;
  writerActive = true;
  writerKind = "sync";
  writerCancel = null;
  busy = true;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  el("entries").hidden = true;
  el("review-items").replaceChildren();
  el("review-message").textContent = "Waiting for native confirmation…";
  el("review-message").focus();
  controls();
  let shouldRefresh = false;
  let acceptingProgress = true;
  try {
    const progress = new window.__TAURI__.core.Channel();
    progress.onmessage = stage => {
      if (!acceptingProgress || writerCancel || typeof stage !== "string") return;
      const label = syncStages.get(stage);
      if (label) el("review-message").textContent = label;
    };
    const args: { id: string; progress: typeof progress; commitMessage?: string; selectedLines?: string[]; massDelete?: boolean } = { id, progress };
    if (commitMessage !== null) args.commitMessage = commitMessage;
    if (selected) args.selectedLines = selectedLines;
    if (massDelete) args.massDelete = true;
    const result = await invoke("run_reviewed_sync", args);
    acceptingProgress = false;
    if (writerCancel) await writerCancel;
    el("review-message").textContent = `${result.outcome}: ${result.message}`;
    renderReviewItems(result.items);
    if (result.outcome === "terminal_recovery") {
      el("review-message").textContent += " Open a terminal and follow dotman's recovery instructions before continuing.";
      showRecovery(result.message, result.recovery);
    } else if (result.outcome === "cleanup_unresolved") {
      el("review-message").textContent += " Cleanup is unresolved. Check dotman in a terminal before continuing.";
    } else {
      shouldRefresh = result.refresh;
    }
  } catch {
    acceptingProgress = false;
    if (writerCancel) await writerCancel;
    el("review-message").textContent = "Sync request failed. Check dotman in a terminal before refreshing.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    clearCommitEditor();
    controls();
  }
  if (shouldRefresh) await refresh(true);
}

async function applyDecisions() {
  if (busy || cancelPending || !statusLoaded || !decisions.size) return;
  const answers = Array.from(decisions, ([id, choice]) => ({ id, action: choice.action }));
  clearDecisions();
  writerActive = true;
  writerKind = "files";
  writerCancel = null;
  busy = true;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  reviewId = null;
  clearCommitEditor();
  clearDiff();
  el("entries").hidden = true;
  el("review").hidden = true;
  el("decision-results").replaceChildren();
  el("decision-message").textContent = "Waiting for native confirmation…";
  el("decision-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("apply_reviewed_files", { answers });
    if (writerCancel) await writerCancel;
    el("decision-message").textContent = `${result.outcome}: ${result.message}`;
    for (const item of result.items) {
      const line = document.createElement("p");
      line.textContent = `${item.group}: ${item.text}`;
      el("decision-results").append(line);
    }
    if (result.outcome === "terminal_recovery") {
      showRecovery(result.message, result.recovery);
    } else {
      shouldRefresh = result.refresh === true;
    }
  } catch {
    if (writerCancel) await writerCancel;
    el("decision-message").textContent = "Apply request failed. Check dotman in a terminal before refreshing.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

async function approvePendingHooks() {
  if (busy || cancelPending || !statusLoaded || hookReviewId === null || el("entries").hidden) return;
  const id = hookReviewId;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "hooks";
  writerCancel = null;
  busy = true;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  clearDiff();
  el("entries").hidden = true;
  el("hooks-result").hidden = false;
  el("hooks-message").textContent = "Waiting for native confirmation…";
  el("hooks-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("approve_pending_hooks", { id });
    if (writerCancel) await writerCancel;
    el("hooks-message").textContent = `${result.outcome}: ${result.message}`;
    if (result.outcome === "terminal_recovery") showRecovery(result.message, result.recovery);
    else if (result.outcome !== "cleanup_unresolved") shouldRefresh = result.refresh === true;
  } catch {
    if (writerCancel) await writerCancel;
    el("hooks-message").textContent = "Hook review failed. Hooks may have run; check dotman in a terminal.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

async function acceptReviewedIdentity() {
  if (busy || cancelPending || !statusLoaded || identityReviewId === null ||
      page !== "settings" || el("machine-files").hidden || !el("machine-files").open) return;
  const id = identityReviewId;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "identity";
  writerCancel = null;
  busy = true;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  clearDiff();
  el("entries").hidden = true;
  el("identity-result").hidden = false;
  el("identity-message").textContent = "Waiting for native confirmation…";
  el("identity-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("accept_reviewed_identity", { id });
    if (writerCancel) await writerCancel;
    el("identity-message").textContent = `${result.outcome}: ${result.message}`;
    if (result.outcome === "terminal_recovery") {
      showRecovery(result.message, result.recovery);
    } else if (result.outcome === "cleanup_unresolved") {
      el("identity-message").textContent += " Cleanup is unresolved. Check dotman in a terminal before continuing.";
    } else {
      shouldRefresh = result.refresh === true;
    }
  } catch {
    if (writerCancel) await writerCancel;
    el("identity-message").textContent = "Identity review request failed. Check dotman in a terminal before refreshing.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

async function runAge(action: "create" | "import") {
  if (busy || cancelPending || !statusLoaded || keyReviewId === null || !el("recovery").hidden) return;
  const id = keyReviewId;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "age";
  writerCancel = null;
  busy = true;
  statusLoaded = false;
  clearMachineFiles();
  clearDiff();
  el("identity-result").hidden = false;
  el("identity-message").textContent = "Waiting for native identity review…";
  el("identity-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await runKey(invoke, id, action);
    if (writerCancel) await writerCancel;
    el("identity-message").textContent = `${result.outcome}: ${result.message}`;
    shouldRefresh = result.refresh === true;
  } catch {
    if (writerCancel) await writerCancel;
    el("identity-message").textContent = "Identity operation could not be verified; check status before trying again.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

async function runSetup(kind: "fresh" | "clone" | "import") {
  if (busy || cancelPending || setupReviewId === null || el("setup").hidden || !el("recovery").hidden) return;
  const id = setupReviewId;
  const source = el("clone-source").value;
  if (kind === "clone" && !source) return;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "setup";
  writerCancel = null;
  busy = true;
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  clearDiff();
  el("setup").hidden = true;
  el("setup-result").hidden = false;
  el("setup-message").textContent = kind === "import" ? "Choose a directory in the native dialog…" : "Waiting for native confirmation…";
  el("setup-message").focus();
  controls();
  if (kind === "clone" && source.length > 8192) {
    busy = false;
    writerActive = false;
    writerKind = null;
    el("setup-message").textContent = "Clone source exceeds 8192 characters. Refresh status to try again.";
    controls();
    return;
  }
  let shouldRefresh = false;
  try {
    const result = kind === "clone"
      ? await invoke("run_clone_setup", { id, source })
      : kind === "import"
        ? await invoke("run_import_setup", { id, action: "import" })
        : await invoke("run_fresh_setup", { id });
    if (writerCancel) await writerCancel;
    el("setup-message").textContent = `${result.outcome}: ${result.message}`;
    if (result.outcome === "terminal_recovery") {
      showRecovery(result.message, result.recovery);
    } else if (result.outcome === "cleanup_unresolved") {
      el("setup-message").textContent += " Cleanup is unresolved. Check dotman in a terminal before continuing.";
    } else {
      shouldRefresh = result.refresh === true;
    }
  } catch {
    if (writerCancel) await writerCancel;
    el("setup-message").textContent = "Setup request failed. The repository may already exist; check in a terminal before continuing.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

async function closeReview() {
  if (writerActive) return;
  if (busy) { void cancel(); return; }
  clearLines();
  const token = ++serial;
  const id = reviewId;
  reviewId = null;
  controls();
  el("review").hidden = true;
  el("review-items").replaceChildren();
  clearCommitEditor();
  el("context").hidden = !el("paths").children.length;
  if (statusLoaded) {
    el("entries").hidden = false;
    el("open-review").focus({ preventScroll: true });
    window.scrollTo(...previousScroll);
  } else {
    el("message").focus();
  }
  selectedRow = null;
  setPage("files");
  if (id !== null) {
    try {
      await invoke("discard_review", { id });
    } catch {
      if (token === serial) el("message").textContent = "Could not discard preview. Refresh status before continuing.";
    }
  }
}

function setPage(next: "files" | "review" | "settings") {
  page = next;
  document.body.setAttribute("data-page", next);
  el("settings").hidden = next !== "settings";
  for (const name of ["files", "review", "settings"] as const) {
    if (name === next) el(`nav-${name}`).setAttribute("aria-current", "page");
    else el(`nav-${name}`).removeAttribute("aria-current");
  }
  el("page-heading").textContent = next === "files" ? "Files" : next === "review" ? "Review" : "Settings";
}

function navigate(next: "files" | "review" | "settings") {
  if (busy || cancelPending || next === page) return;
  if (page === "review") {
    if (diffOrigin?.review) {
      clearDiff();
      diffOrigin = null;
    }
    void closeReview();
  } else if (diffOrigin) {
    clearDiff();
    rowNodes.get(diffOrigin.id)?.setAttribute("aria-current", "false");
    diffOrigin = null;
  }
  setPage(next);
  el("page-heading").focus();
  if (next === "review") {
    if (statusLoaded) void openReview();
    else {
      el("review").hidden = false;
      el("review-message").textContent = "Status is unavailable. Refresh status before opening an outgoing preview.";
      controls();
    }
  } else if (next === "files") {
    el("entries").hidden = !statusLoaded;
  }
}

for (const name of ["files", "review", "settings"] as const) {
  el(`nav-${name}`).addEventListener("click", () => navigate(name));
}
el("refresh").addEventListener("click", () => {
  if (page === "review" && !busy && !cancelPending) {
    void closeReview();
    el("message").focus();
  }
  void refresh();
});
for (const id of ["setup-check", "recovery-check"]) {
  el(id).addEventListener("click", async () => {
    if (busy || cancelPending) return;
    const button = el(id);
    const hadFocus = document.activeElement === button;
    if (hadFocus) el("message").focus();
    await refresh();
    const panel = id === "setup-check" ? el("setup") : el("recovery");
    if (hadFocus && !panel.hidden && document.activeElement === el("message")) {
      button.focus();
    }
  });
}
el("cancel").addEventListener("click", cancel);
el("close-diff").addEventListener("click", closeDiff);
el("replacement-batch").addEventListener("click", () => {
  if (busy || cancelPending || !replacementReview || !statusLoaded ||
      replacementReview.token !== serial || !entries.includes(replacementReview.entry) ||
      diffOrigin?.id !== replacementReview.entry.id || diffOrigin.review || el("diff").hidden) return;
  const entry = replacementReview.entry;
  replacementReview = null;
  el("replacement-review").hidden = true;
  decisions.set(entry.id, { action: "take_repo", key: entry.key });
  renderDecisions();
  el("decisions-title").focus({ preventScroll: true });
  el("decisions").scrollIntoView({ block: "start" });
});
el("replacement-back").addEventListener("click", () => {
  if (busy || cancelPending || !replacementReview) return;
  const { launcher } = replacementReview;
  replacementReview = null;
  el("replacement-review").hidden = true;
  launcher.focus();
});
el("search").addEventListener("input", renderRows);
for (const [id, folders] of [["group-folders", true], ["group-status", false]] as const) {
  el(id).addEventListener("click", () => {
    folderMode = folders;
    el("group-folders").setAttribute("aria-pressed", String(folders));
    el("group-status").setAttribute("aria-pressed", String(!folders));
    renderRows();
  });
}
el("next-undecided").addEventListener("click", () => {
  if (busy || cancelPending || !statusLoaded || !diffOrigin || diffOrigin.review || el("diff").hidden) return;
  const entry = nextUndecided();
  if (entry) void openDiff(entry, rowNodes.get(entry.id));
});
el("theme-mode").addEventListener("change", () => {
  document.documentElement.setAttribute("data-theme", el("theme-mode").value);
});
el("accent-mode").addEventListener("change", () => {
  document.documentElement.setAttribute("data-accent", el("accent-mode").value);
});
el("open-review").addEventListener("click", () => navigate("review"));
el("close-review").addEventListener("click", closeReview);
el("sync-reviewed").addEventListener("click", () => syncReviewed());
el("sync-mass-delete").addEventListener("click", () => syncReviewed(false, true));
el("sync-lines").addEventListener("click", () => syncReviewed(true));
el("leave-lines").addEventListener("click", () => { if (!busy && !cancelPending) clearLines(); });
el("commit-message").addEventListener("input", updateCommitValidation);
el("clear-decisions").addEventListener("click", clearDecisions);
el("apply-decisions").addEventListener("click", applyDecisions);
el("machine-files").addEventListener("toggle", renderMachineFiles);
el("review-identity").addEventListener("click", acceptReviewedIdentity);
el("review-hooks").addEventListener("click", approvePendingHooks);
el("start-fresh").addEventListener("click", () => runSetup("fresh"));
el("start-import").addEventListener("click", () => runSetup("import"));
el("create-age").addEventListener("click", () => runAge("create"));
el("import-age").addEventListener("click", () => runAge("import"));
el("clone-source").addEventListener("input", controls);
el("start-clone").addEventListener("click", () => runSetup("clone"));
policyControls({
  invoke,
  getId: () => policyReviewId,
  canStart: () => !busy && !cancelPending && statusLoaded && page === "settings",
  begin: () => {
    policyReviewId = null;
    statusLoaded = false;
    entries = [];
    clearDecisions();
    clearLines();
    clearDiff();
    clearMachineFiles();
    reviewId = null;
    el("entries").hidden = true;
    writerActive = true;
    writerKind = "policy";
    writerCancel = null;
    busy = true;
    controls();
  },
  finish: async shouldRefresh => {
    if (writerCancel) await writerCancel;
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
    if (shouldRefresh) await refresh();
  },
});
void refresh();
