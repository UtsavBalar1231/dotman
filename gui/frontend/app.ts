import { browserPosition, fileFocusKey } from "./browser-position.js";
import { displayStatus, matchesSearch } from "./file-status.js";
import { trackingFlow } from "./tracking.js";
import { editSource } from "./source-editor.js";
import { pathLabel, render, reveal } from "./file-tree.js";
import { clear, show } from "./diff-reader.js";
import { runKey } from "./keys.js";
import { policyControls } from "./policy-trust.js";
import { getWorkspaceView, updateWorkspace } from "./workspace-view.js";
import { icon } from "./fluent-icons.js";
import type { DesktopWindow, Destination, DestinationChange, EntryView, FileAction, LineEdit, MachineDetails, RecoveryRow, ReviewItem, TauriCore } from "./protocol.js";
import type { Page } from "./workspace-view.js";

declare global {
  interface Window {
    __TAURI__: { core: TauriCore; window: { getCurrentWindow(): DesktopWindow } };
  }
}

const invoke = window.__TAURI__.core.invoke;
type InputId = "search" | "clone-source";
type ButtonId = "open-repository" | "open-recovery-repository" | "open-installation" | "retry-status" | "open-destination" | "dismiss-held" | "review-return" | "cancel-review" | "refresh" | "cancel" | "start-fresh" | "start-clone" | "start-import" | "setup-check" | "setup-retry" | "recovery-check" | "create-age" | "import-age" | "review-identity" | "review-hooks" | "sync-reviewed" | "sync-mass-delete" | "sync-lines" | "leave-lines" | "close-review" | "apply-decisions" | "clear-decisions" | "review-decisions" | "close-decisions" | "retry-decisions" | "next-undecided" | "close-diff" | "group-folders" | "group-status";
type ElementFor<T extends string> = T extends InputId ? HTMLInputElement
  : T extends ButtonId ? HTMLButtonElement
  : T extends "commit-message" ? HTMLTextAreaElement
  : T extends "machine-files" | "commit-editor" ? HTMLDetailsElement
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
let previousScroll: [number, number] = [0, 0];
let diffOrigin: {
  review: boolean;
  row: HTMLElement | undefined;
  id: string;
  scroll: [number, number];
  queueScroll: number;
  reviewScroll: number;
} | null = null;
const rowNodes = new Map<string, HTMLElement>();
const markedChoices = new Map<string, FileAction>();
let folderMode = true;
let statusLoaded = false;
let statusLocations: { home: string; repo: string } | null = null;
let reviewId: string | null = null;
let massDeleteReview = false;
let writerActive = false;
let writerCancel: Promise<void> | null = null;
let suggestedMessage: string | null = null;
const writerFeedback = {
  files: ["apply", "decision-message"],
  variant: ["host-variant review", "variant-message"],
  held: ["held-file review", "held-message"],
  identity: ["identity review", "identity-message"],
  hooks: ["hook review", "hooks-message"],
  setup: ["setup", "setup-message"],
  sync: ["sync", "review-message"],
  age: ["identity operation", "identity-message"],
  policy: ["policy review", "policy-message"],
  tracking: ["tracking", "tracking-message"],
  destination: ["destination change", "message"],
} as const;
let writerKind: keyof typeof writerFeedback | null = null;
const decisions = new Map<string, { action: FileAction; key: string }>();
let decisionsOpen = false;
let retryDecisions = false;
let readingDiff = false;
let backAfterDiff = false;
let diffChoiceId: string | null = null;
let clearChoiceButton: HTMLButtonElement | null = null;
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
  ["keep_local", "Keep this computer’s copy"], ["take_repo", "Use repository copy"], ["restore", "Restore on this computer"],
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
  let choicesChanged = false;
  for (const id of new Set([...markedChoices.keys(), ...decisions.keys()])) {
    const choice = decisions.get(id)?.action;
    if (markedChoices.get(id) === choice) continue;
    choicesChanged = true;
    const row = rowNodes.get(id);
    if (row) showRowChoice(row, choice);
    if (choice) markedChoices.set(id, choice);
    else markedChoices.delete(id);
  }
  if (choicesChanged && el("search").value.trim()) renderRows();
  updateComparisonLabels();
  const selected = diffChoiceId === null ? undefined : decisions.get(diffChoiceId)?.action;
  for (const [action, button] of diffChoiceButtons) {
    button.setAttribute("aria-pressed", action === selected ? "true" : "false");
  }
  el("diff-selection").textContent = selected && diffChoiceButtons.size ? `Selected: ${actionLabels.get(selected)}` : "";
  if (clearChoiceButton) clearChoiceButton.hidden = selected === undefined;
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
  el("commit-editor").open = false;
  el("commit-preview").textContent = "";
  el("commit-error").textContent = "";
  el("commit-message").setAttribute("aria-invalid", "false");
}

function defaultCommitMessage() {
  return selectedOnly ? selectedDefaultMessage : suggestedMessage;
}

function updateCommitPreview() {
  const draft = el("commit-message").value;
  el("commit-preview").textContent = commitError() ? "Check message" : draft.split("\n", 1)[0];
  el("restore-commit-message").hidden = draft === defaultCommitMessage();
}

function commitError() {
  if (suggestedMessage === null || el("commit-message").value === defaultCommitMessage()) return "";
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
  if (suggestedMessage !== null) updateCommitPreview();
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
  const hasDecisionResult = Boolean(el("decision-message").textContent);
  if (!decisions.size && !hasDecisionResult) decisionsOpen = false;
  el("decisions").hidden = !decisionsOpen;
  const queueLabel = decisions.size
    ? `Review ${decisions.size} ${decisions.size === 1 ? "decision" : "decisions"}`
    : "View file decision result";
  const queued = el("decisions").hidden && (decisions.size || hasDecisionResult) && getWorkspaceView().page === "files"
    ? queueLabel : null;
  el("close-diff").disabled = cancelPending || (busy && (!readingDiff || backAfterDiff));
  const cancelLabel = writerActive && writerKind !== null ? `Cancel ${writerFeedback[writerKind][0]}` : "Cancel read";
  updateWorkspace({ busy: busy || cancelPending, cancelling: cancelPending, queued, cancel: busy || cancelPending ? cancelLabel : null });
  const trackMenu = el("track-menu") as HTMLDetailsElement;
  trackMenu.inert = busy || cancelPending || !statusLoaded;
  if (trackMenu.inert) trackMenu.open = false;
  const stopTracking = el("tracking-file-action").querySelector("button");
  if (stopTracking) stopTracking.disabled = busy || cancelPending || !statusLoaded;
  const edit = el("source-edit-details").querySelector("button");
  if (edit) edit.disabled = busy || cancelPending;
  el("open-destination").disabled = busy || cancelPending || !statusLoaded || !getWorkspaceView().repository;
  el("dismiss-held").disabled = writerKind === "held";
  el("close-decisions").disabled = busy || cancelPending;
  el("retry-decisions").hidden = !retryDecisions;
  el("retry-decisions").disabled = busy || cancelPending;
  el("recovery-check").disabled = busy || cancelPending;
  el("setup-check").disabled = busy || cancelPending;
  el("retry-status").disabled = busy || cancelPending;
  el("open-installation").disabled = busy || cancelPending;
  el("open-repository").disabled = busy || cancelPending;
  el("open-recovery-repository").disabled = busy || cancelPending;
  el("setup-retry").disabled = busy || cancelPending;
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
  el("sync-reviewed").hidden = reviewId === null || selectedOnly || massDeleteReview;
  el("sync-reviewed").disabled = busy || cancelPending || !!commitError();
  el("sync-mass-delete").hidden = reviewId === null || !massDeleteReview;
  el("sync-mass-delete").disabled = busy || cancelPending || !!commitError();
  el("sync-lines").hidden = reviewId === null || !selectedOnly;
  el("sync-lines").disabled = busy || cancelPending || !!commitError() || !Array.from(lineReceipts.values()).some(receipt => receipt.selected.size);
  el("leave-lines").disabled = busy || cancelPending;
  el("review-return").disabled = busy || cancelPending;
  el("close-review").disabled = writerActive || (!el("review-return").hidden && busy);
  el("cancel-review").hidden = !writerActive || writerKind !== "sync";
  el("cancel-review").disabled = cancelPending;
  el("apply-decisions").disabled = busy || cancelPending || !statusLoaded || !decisions.size;
  el("clear-decisions").disabled = busy || cancelPending || !decisions.size;
}

function clearDiff() {
  el("diff").hidden = true;
  el("diff-key").textContent = "";
  el("diff-context").textContent = "";
  el("diff-message").textContent = "";
  clear();
  el("source-edit-details").replaceChildren();
  el("diff-actions").replaceChildren();
  el("diff-selection").textContent = "";
  el("line-edits").hidden = true;
  el("line-edit-list").replaceChildren();
  diffChoiceId = null;
  clearChoiceButton = null;
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
  ["decision", "Needs a decision"], ["outgoing", "Changed on this computer"],
  ["incoming", "Changed in repository"], ["held", "Held"],
  ["unavailable", "Cannot be compared or applied"], ["other", "Other status"],
];

const holdOrigins = new Map([
  ["new_file", "New file"], ["home_edit", "Home edit"], ["repo_edit", "Repository edit"],
]);

function renderHeldCounts(counts: { new_files: number; home_edits: number; repo_edits: number } | undefined) {
  const labels = [["new_files", "new file"], ["home_edits", "home edit"], ["repo_edits", "repository edit"]] as const;
  const parts: string[] = [];
  for (const [key, label] of labels) {
    const count = counts?.[key];
    if (typeof count === "number" && Number.isSafeInteger(count) && count > 0) {
      parts.push(`${count} ${label}${count === 1 ? "" : "s"}`);
    }
  }
  el("held-counts").hidden = !parts.length;
  el("held-counts").textContent = parts.length ? `Held across repository · ${parts.join(" · ")}` : "";
}

const heldActions = [
  ["add", "Review tracking…"], ["plain", "Store unencrypted…"],
  ["encrypt", "Encrypt…"], ["forget", "Stop tracking…"],
] as const;
type HeldAction = typeof heldActions[number][0];

function heldRow(entry: EntryView, displayName: string) {
  const details = document.createElement("details");
  details.className = "held-row";
  details.setAttribute("data-held-key", fileFocusKey(entry));
  const summary = document.createElement("summary");
  summary.className = "row";
  const status = document.createElement("span");
  status.className = "row-status";
  status.textContent = displayStatus(entry.status);
  summary.setAttribute("aria-label", `${entry.key}. ${status.textContent}. Hold details.`);
  summary.title = entry.key;
  summary.append(icon("chevron-right", "disclosure-glyph"), pathLabel(displayName), status);
  details.append(summary);
  details.addEventListener("toggle", () => {
    if (details.open) clearVariantResult();
    if (!details.open || details.children.length > 1) return;
    const hold = entry.hold;
    const body = document.createElement("div");
    body.className = "held-details";
    const reason = document.createElement("strong");
    reason.textContent = typeof hold?.reason === "string" && hold.reason ? hold.reason : "Held for review";
    body.append(reason);
    const metadata = document.createElement("dl");
    for (const [label, value] of [
      ["Origin", holdOrigins.get(hold?.origin ?? "")], ["Rule", hold?.rule], ["Location", hold?.location],
    ] as const) {
      if (typeof value !== "string" || !value) continue;
      const term = document.createElement("dt");
      term.textContent = label;
      const description = document.createElement("dd");
      description.textContent = value;
      metadata.append(term, description);
    }
    if (metadata.children.length) body.append(metadata);
    if (hold?.guidance) {
      const note = document.createElement("p");
      note.textContent = hold.guidance;
      body.append(note);
    }
    const actions = document.createElement("div");
    actions.className = "held-actions";
    for (const [action, label] of heldActions) {
      if (!entry.held_actions?.includes(action)) continue;
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = label;
      button.addEventListener("click", () => runHeld(entry, action));
      actions.append(button);
    }
    if (actions.children.length) body.append(actions);
    else {
      const note = document.createElement("p");
      note.textContent = "No action available for this hold.";
      body.append(note);
    }
    details.append(body);
  });
  return details;
}

async function runHeld(entry: EntryView, action: HeldAction) {
  if (busy || cancelPending || !statusLoaded || !el("recovery").hidden) return;
  if (!entries.includes(entry) || !entry.held_actions?.includes(action)) return;
  clearDecisions();
  clearCommitEditor();
  reviewId = null;
  writerActive = true;
  writerKind = "held";
  writerCancel = null;
  busy = true;
  statusLoaded = false;
  el("held-result").hidden = false;
  el("held-message").textContent = "Waiting for your review…";
  el("held-title").textContent = `Held file · ${entry.key}`;
  el("held-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("run_held", { id: entry.id, action });
    if (writerCancel) await writerCancel;
    el("held-message").textContent = result.message;
    shouldRefresh = result.refresh === true;
  } catch {
    el("held-message").textContent = "Could not verify the result. Refresh status before trying again.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (shouldRefresh) await refresh();
}

function queuedLabel(action: FileAction) {
  return `Queued · ${actionLabels.get(action)}`;
}

function matchesFile(entry: EntryView, query: string) {
  if (matchesSearch(entry, query)) return true;
  const choice = decisions.get(entry.id);
  return Boolean(choice && queuedLabel(choice.action).toLocaleLowerCase().includes(query));
}

function showRowChoice(row: HTMLElement, choice: FileAction | undefined) {
  const status = row.lastElementChild as HTMLElement;
  const detail = choice ? queuedLabel(choice) : status.getAttribute("data-status") ?? "";
  status.textContent = choice ? "Queued" : detail;
  status.title = detail;
  status.hidden = !choice && status.getAttribute("data-repeats-group") === "true";
  row.setAttribute("aria-label", `${row.title} — ${detail}`);
  row.setAttribute("data-queued", String(Boolean(choice)));
}

function fileLeaf(entry: EntryView, displayName: string) {
  const item = document.createElement("li");
  if (entry.group === "held") {
    const details = heldRow(entry, displayName);
    const focusTarget = folderMode ? item : details.querySelector("summary");
    focusTarget?.setAttribute("data-file-focus", fileFocusKey(entry));
    item.append(details);
    if (folderMode) item.setAttribute("aria-label", `${entry.key} — ${displayStatus(entry.status)}`);
    return item;
  }
  let row: HTMLElement = item;
  if (!folderMode) {
    const button = document.createElement("button");
    button.type = "button";
    item.append(button);
    row = button;
  }
  row.className = "row";
  row.setAttribute("data-group", entry.group);
  row.setAttribute("data-file-focus", fileFocusKey(entry));
  row.setAttribute("aria-current", diffOrigin?.id === entry.id ? "true" : "false");
  rowNodes.set(entry.id, row);
  const status = document.createElement("span");
  status.className = "row-status";
  if (entry.status === "changed-in-repo" && !entry.system && entry.supported) {
    status.className += " repository-change";
    status.textContent = displayStatus(entry.status);
  } else {
    status.textContent = `${displayStatus(entry.status)}${entry.system ? " · system (read only)" : ""}${entry.supported ? "" : " · diff unavailable"}`;
  }
  const repeatsGroup = (entry.group === "outgoing" && entry.status === "changed-locally")
    || (entry.group === "incoming" && entry.status === "changed-in-repo");
  status.setAttribute("data-repeats-group", String(!folderMode && !entry.system && entry.supported && repeatsGroup));
  status.setAttribute("data-status", status.textContent);
  row.title = entry.key;
  row.append(pathLabel(displayName), status);
  showRowChoice(row, decisions.get(entry.id)?.action);
  row.addEventListener("click", () => openDiff(entry, row));
  return item;
}

async function runHostVariant(id: string) {
  if (busy || cancelPending || !statusLoaded || diffOrigin?.id !== id) return;
  busy = true;
  writerActive = true;
  writerKind = "variant";
  writerCancel = null;
  controls();
  el("variant-result").hidden = false;
  el("variant-message").textContent = "Waiting for native host-variant review…";
  let shouldRefresh = false;
  try {
    const result = await invoke("run_host_variant", { id });
    if (writerCancel) await writerCancel;
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
  if (shouldRefresh) await refresh({ preserveVariantResult: true });
}

const rowBatchSize = 200;

function populateRows(list: HTMLUListElement, rows: readonly EntryView[], root: HTMLElement, focused?: EntryView) {
  let drawn = 0;
  const drawBatch = () => {
    const size = !drawn && focused ? Math.max(rowBatchSize, rows.indexOf(focused) + 1) : rowBatchSize;
    const end = Math.min(drawn + size, rows.length);
    for (const entry of rows.slice(drawn, end)) list.append(fileLeaf(entry, entry.key));
    drawn = end;
    if (drawn < rows.length) {
      window.requestAnimationFrame(() => {
        if (root.contains(list)) drawBatch();
      });
    }
  };
  drawBatch();
}

let selectedFolder: string | null = null;
function selectTreeFolder(path: string | null) {
  selectedFolder = path;
  const button = el("untrack-folder");
  button.textContent = path ? "Stop tracking this folder…" : "Stop tracking a folder…";
  button.title = path ?? "Choose a managed folder";
}

function showSearchCount(query: string, count: number) {
  el("search-count").textContent = query ? `${count} ${count === 1 ? "match" : "matches"}` : "";
  el("clear-search").hidden = !el("search").value;
}

function clearSearch() {
  el("search").value = "";
  renderRows();
  el("search").focus();
}

function renderRows(focused?: EntryView) {
  selectTreeFolder(null);
  const root = el("rows");
  root.replaceChildren();
  rowNodes.clear();
  const query = el("search").value.trim().toLocaleLowerCase();
  if (folderMode) {
    const count = render(root, entries, query, fileLeaf, matchesFile);
    root.setAttribute("tabindex", count ? "-1" : "0");
    showSearchCount(query, count);
    if (!count) {
      root.textContent = query ? "No files match this search." : "No files tracked yet.";
      if (!query) {
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = "Track your first file…";
        button.addEventListener("click", () => void startTracking("track_files"));
        root.append(button);
      }
    }
    return;
  }
  root.setAttribute("tabindex", "0");
  const matches = new Map<string, EntryView[]>();
  for (const entry of entries) {
    if (entry.group === "clean" || !matchesFile(entry, query)) continue;
    const group = matches.get(entry.group);
    if (group) group.push(entry);
    else matches.set(entry.group, [entry]);
  }
  let visible = 0;
  for (const [group, label] of groups) {
    const found = matches.get(group);
    if (!found) continue;
    visible += found.length;
    const section = document.createElement("section");
    const heading = document.createElement("h3");
    heading.textContent = `${label} (${found.length})`;
    const list = document.createElement("ul");
    section.append(heading, list);
    root.append(section);
    populateRows(list, found, root, focused);
  }
  showSearchCount(query, visible);
  if (!visible) {
    const empty = document.createElement("p");
    empty.textContent = query ? "No files match this search." : "No files need attention.";
    root.append(empty);
  }
}

function needsChoice(entry: EntryView) {
  return entry.group === "decision" && entry.supported && !entry.system;
}

function nextUndecided() {
  if (!entries.length || !diffOrigin) return null;
  const id = diffOrigin.id;
  const start = entries.findIndex(entry => entry.id === id);
  for (let step = 1; step < entries.length; step++) {
    const entry = entries[(start + step) % entries.length];
    if (needsChoice(entry) && !decisions.has(entry.id)) return entry;
  }
  return null;
}

const comparisonRows = new Map<string, { entry: EntryView; button: HTMLButtonElement; label: HTMLElement }>();
const comparisonPageSize = 6;
let comparisonEntries: EntryView[] = [];
let decisionOverview = false;

function updateComparisonLabels() {
  if (decisionOverview) {
    const total = comparisonEntries.length;
    const queued = comparisonEntries.reduce((count, entry) => count + Number(decisions.has(entry.id)), 0);
    el("guide-description").textContent = queued
      ? `${total - queued} to inspect · ${queued} queued for review`
      : `${total} ${total === 1 ? "file needs" : "files need"} a choice between the two copies.`;
  }
  for (const { entry, button, label } of comparisonRows.values()) {
    const choice = decisions.get(entry.id);
    const detail = choice ? queuedLabel(choice.action) : displayStatus(entry.status);
    label.textContent = detail;
    label.setAttribute("data-queued", String(Boolean(choice)));
    button.setAttribute("aria-label", `Compare ${entry.key} — ${detail}`);
    button.title = `${entry.key} — ${detail}`;
  }
}

function appendComparisons(focusNew = false, count = comparisonPageSize) {
  const start = comparisonRows.size;
  const batch = comparisonEntries.slice(start, start + count);
  for (const entry of batch) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    const label = document.createElement("span");
    label.className = "comparison-state";
    button.setAttribute("data-file-focus", fileFocusKey(entry));
    button.append(pathLabel(entry.key), label, icon("chevron-right"));
    button.addEventListener("click", () => openDiff(entry, button));
    item.append(button);
    el("attention-list").append(item);
    comparisonRows.set(entry.id, { entry, button, label });
  }
  updateComparisonLabels();
  const remaining = comparisonEntries.length - comparisonRows.size;
  const more = el("more-comparisons");
  more.hidden = !remaining;
  more.textContent = `Show ${Math.min(remaining, comparisonPageSize)} more`;
  if (focusNew && batch[0]) comparisonRows.get(batch[0].id)?.button.focus();
}

function renderWorklist(count = comparisonPageSize, focused?: EntryView) {
  comparisonRows.clear();
  el("attention-list").replaceChildren();
  comparisonEntries = entries.filter(needsChoice);
  decisionOverview = comparisonEntries.length > 0;
  if (!decisionOverview) {
    comparisonEntries = entries.filter(entry => entry.supported && (entry.group === "incoming" || entry.group === "outgoing"));
  }
  el("guide-title").textContent = decisionOverview ? "File decisions" : "File comparison";
  if (!decisionOverview) {
    el("guide-description").textContent = comparisonEntries.length
      ? `${comparisonEntries.length} changed ${comparisonEntries.length === 1 ? "file" : "files"} to compare.`
      : entries.some(entry => entry.group !== "clean") || !el("held-counts").hidden
        ? "Select a file in the tree to see its details."
        : "No files need attention.";
  }
  appendComparisons(false, focused ? Math.max(count, comparisonEntries.indexOf(focused) + 1) : count);
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

const readIssues = new Map([
  ["companion_unavailable", ["Dotman couldn’t start its CLI", "The desktop app needs the matching dotman executable in its installation folder. Check the installed pair, then retry."]],
  ["invalid_report", ["Dotman couldn’t read the status report", "The CLI returned an incomplete or incompatible report. Check that the desktop app and CLI come from the same build."]],
  ["timeout", ["Status check timed out", "The CLI did not finish within 30 seconds. Retry when the repository is ready."]],
  ["cancelled", ["Status check cancelled", "Retry to load the current files and changes."]],
]);

function reportReadIssue(finding: string) {
  const [title, detail] = readIssues.get(finding) ?? ["File status unavailable", "Dotman couldn’t verify the current files and changes. Retry to read the repository again."];
  if (!el("recovery").hidden) {
    el("recovery-retry-feedback").textContent = detail;
    return;
  }
  if (!el("setup").hidden) return;
  el("retry-status").hidden = false;
  el("status-issue-title").textContent = title;
  el("status-issue-description").textContent = detail;
  el("open-installation").hidden = finding !== "companion_unavailable" && finding !== "invalid_report";
  el("status-issue").hidden = false;
}

async function refresh({ preserveReview = false, preserveVariantResult = false, keepFilesVisible = false } = {}) {
  if (busy || cancelPending) return;
  const setupWasVisible = !el("setup").hidden;
  el("installation-feedback").textContent = "";
  el("repository-folder-feedback").textContent = "";
  el("recovery-folder-feedback").textContent = "";
  el("open-recovery-repository").hidden = true;
  el("recovery-retry-feedback").textContent = "";
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
    displayReview(false);
    el("review-items").replaceChildren();
  }
  reviewId = null;
  if (!keepFilesVisible) entries = [];
  statusLoaded = false;
  statusLocations = null;
  clearMachineFiles();
  el("setup").hidden = true;
  if (!keepFilesVisible) el("entries").hidden = true;
  el("context").hidden = true;
  el("paths").replaceChildren();
  el("repository-path").textContent = "";
  updateWorkspace({ status: "Loading status…" });
  const showReading = () => {
    if (token !== serial || preserveReview || keepFilesVisible || setupWasVisible || !el("recovery").hidden) return;
    el("status-issue-title").textContent = "Reading repository status…";
    el("status-issue-description").textContent = "Comparing the repository with files on this computer.";
    el("open-installation").hidden = true;
    el("retry-status").hidden = true;
    el("status-issue").hidden = false;
  };
  if (!el("status-issue").hidden) showReading();
  const loadingTimer = setTimeout(showReading, 350);
  try {
    const view = await invoke("refresh");
    if (token !== serial) return;
    el("status-issue").hidden = true;
    const recoveryStatus = ["in_progress", "pull_conflict", "stopped"].includes(view.finding);
    if (view.success) {
      el("recovery-finding").textContent = "";
      el("recovery").hidden = true;
      el("recovery-message").textContent = "";
      el("recovery-steps").replaceChildren();
      el("recovery-details").hidden = true;
    } else if (recoveryStatus) {
      showRecovery("Repository status needs attention.");
    }
    el("notice").hidden = !view.key_stripped;
    updateWorkspace({ status: !view.success && recoveryStatus
      ? "Repository status needs attention." : view.message });
    updateWorkspace({ repository: view.context?.repo ?? "" });
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
    el("open-recovery-repository").hidden = !recoveryStatus || !view.context;
    if (recoveryStatus) el("recovery-finding").textContent = view.finding === "in_progress"
      ? "Dotman reports an operation in progress."
      : view.finding === "pull_conflict"
        ? "Dotman reports a pull conflict."
        : "Dotman reports a stopped operation; its cause is not available here.";
    const position = keepFilesVisible && view.success
      ? browserPosition(el("rows"), el("comparison-guide")) : null;
    const shown = position ? Math.max(comparisonPageSize, comparisonRows.size) : comparisonPageSize;
    entries = view.entries;
    statusLoaded = view.success;
    statusLocations = view.success && view.context ? { home: view.context.home, repo: view.context.repo } : null;
    machineDetails = view.success ? view.context?.machine_details ?? null : null;
    identityReviewId = view.success && typeof view.identity_review_id === "string" && view.identity_review_id.length ? view.identity_review_id : null;
    policyReviewId = view.success && typeof view.policy_review_id === "string" && view.policy_review_id.length ? view.policy_review_id : null;
    keyReviewId = view.success && typeof view.key_review_id === "string" && view.key_review_id.length ? view.key_review_id : null;
    hookReviewId = view.success && typeof view.hook_review_id === "string" && view.hook_review_id.length ? view.hook_review_id : null;
    el("setup").hidden = !(!view.success && view.repo_unavailable === true && view.context && !preserveReview && el("recovery").hidden);
    if (!el("setup").hidden || !el("recovery").hidden || statusLoaded) el("setup-retry").hidden = true;
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
    el("open-destination").setAttribute("aria-label", `View sync destination. ${view.remote}`);
    el("clean").textContent = `${view.clean} clean`;
    renderHeldCounts(view.held_counts);
    const focused = position?.file ? entries.find(entry => fileFocusKey(entry) === position.file) : undefined;
    renderRows(position?.inOverview ? undefined : focused);
    renderWorklist(shown, position?.inOverview ? focused : undefined);
    position?.restore(el("search"));
    el("entries").hidden = !view.success;
    if (!view.success && !recoveryStatus) reportReadIssue(view.finding);
  } catch {
    if (token === serial) {
      entries = [];
      statusLoaded = false;
      statusLocations = null;
      clearMachineFiles();
      el("entries").hidden = true;
      el("context").hidden = true;
      updateWorkspace({ status: "Status read failed.", repository: "" });
      if (setupWasVisible && el("recovery").hidden) {
        el("setup").hidden = false;
        el("setup-finding").textContent = "The repository status could not be verified.";
        el("setup-guidance").textContent = "Setup is not available until a successful check permits it.";
        el("setup-paths").replaceChildren();
      } else reportReadIssue("unknown");
    }
  } finally {
    clearTimeout(loadingTimer);
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
  diffOrigin = {
    review: review !== null, row, id: entry.id,
    scroll: [window.scrollX, window.scrollY],
    queueScroll: el("rows").scrollTop,
    reviewScroll: el("review-body").scrollTop,
  };
  clearDiff();
  if (review === null) rowNodes.get(entry.id)?.setAttribute("aria-current", "true");
  if (diffOrigin.review) displayReview(false);
  if (window.matchMedia?.("(max-width: 56rem)").matches && document.activeElement instanceof HTMLElement) {
    document.activeElement.blur();
  }
  el("diff").hidden = false;
  const backLabel = document.createElement("span");
  backLabel.textContent = review === null ? "Back to files" : "Back to review";
  const back = el("close-diff");
  back.setAttribute("aria-label", backLabel.textContent);
  back.title = backLabel.textContent;
  back.replaceChildren(icon("back"), backLabel);
  el("diff-key").textContent = entry.key;
  el("diff-context").textContent = review !== null ? "Capture preview" : displayStatus(entry.status);
  if (review === null && !entry.system && entry.group !== "held" && entry.status !== "new") {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Stop tracking…";
    button.addEventListener("click", () => void startTracking("stop_tracking", entry.key));
    el("tracking-file-action").replaceChildren(button);
  } else {
    el("tracking-file-action").replaceChildren();
  }
  if (review === null && entry.host_variant) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Create host variant";
    button.addEventListener("click", () => runHostVariant(entry.id));
    el("diff-actions").append(button);
  }
  if (review === null && entry.editable) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Edit source";
    button.title = "Edit this file in the repository";
    button.addEventListener("click", () => { if (!busy) void editSource(entry, invoke, async () => {
      await refresh();
      const updated = entries.find(item => item.key === entry.key && item.editable);
      if (updated) await openDiff(updated, rowNodes.get(updated.id));
    }); });
    el("source-edit-details").append(button);
  }
  el("close-diff").focus();
  el("comparison-body").scrollTop = 0;
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
      el("diff-context").textContent = "The repository copy has changed.";
    } else el("diff-context").textContent += ` · Repository vs ${entry.system ? "system" : "home"}`;
    if (typeof recorded === "string" && recorded) el("diff-context").textContent += ` · Recorded source: ${recorded}`;
  }
  const token = ++serial;
  readingDiff = true;
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
          button.append(icon("forward"));
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
        const clearChoice = document.createElement("button");
        clearChoice.type = "button";
        clearChoice.textContent = "Clear choice";
        clearChoice.addEventListener("click", () => {
          if (busy || cancelPending || token !== serial || !statusLoaded || !entries.includes(entry)) return;
          const choice = decisions.get(entry.id);
          if (!choice) return;
          decisions.delete(entry.id);
          renderDecisions();
          diffChoiceButtons.get(choice.action)?.focus({ preventScroll: true });
        });
        clearChoiceButton = clearChoice;
        el("diff-actions").append(clearChoice);
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
      updateWorkspace({ status: "Repository status needs attention." });
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
    readingDiff = false;
    busy = false;
    const returnToFiles = backAfterDiff && token === serial;
    backAfterDiff = false;
    controls();
    if (returnToFiles) closeDiff();
  }
}

async function cancel() {
  if (!busy || cancelPending) return;
  clearLines();
  clearDecisions();
  if (writerActive && writerKind !== null) {
    cancelPending = true;
    const [operation, target] = writerFeedback[writerKind];
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
  displayReview(false);
  el("review-items").replaceChildren();
  clearCommitEditor();
  reviewId = null;
  setPage("files");
  controls();
  entries = [];
  statusLoaded = false;
  clearMachineFiles();
  el("setup").hidden = true;
  el("entries").hidden = true;
  updateWorkspace({ status: "Cancelling read…" });
  el("message").focus();
  controls();
  try {
    await invoke("cancel");
    updateWorkspace({ status: "Read cancelled. Refresh status to continue." });
    reportReadIssue("cancelled");
  } catch {
    updateWorkspace({ status: "Could not cancel read." });
    reportReadIssue("unknown");
  } finally {
    cancelPending = false;
    controls();
  }
}

function closeDiff() {
  if (busy || cancelPending) {
    if (readingDiff && !cancelPending) {
      backAfterDiff = true;
      controls();
    }
    return;
  }
  ++serial;
  clearDiff();
  if (diffOrigin?.review && reviewId !== null) {
    displayReview(true);
    el("review-body").scrollTop = diffOrigin.reviewScroll;
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
    const overview = diffOrigin && comparisonRows.get(diffOrigin.id)?.button;
    row?.setAttribute("aria-current", "false");
    if (overview && overview === diffOrigin?.row && overview.getClientRects().length) {
      overview.focus({ preventScroll: true });
    } else if (row) {
      reveal(row);
      row.focus({ preventScroll: true });
      row.scrollIntoView({ block: "nearest", inline: "nearest" });
    } else {
      el("search").focus();
    }
  } else {
    el("message").focus();
  }
  diffOrigin = null;
}

function displayReview(open: boolean) {
  const panel = el("review") as HTMLDialogElement;
  if (!open) panel.close?.();
  panel.hidden = !open;
  if (open && !panel.open) panel.showModal?.();
}
el("review").addEventListener("cancel", event => { event.preventDefault(); requestCloseReview(); });

function reviewHeading(title: string, phase: "preview" | "running" | "success" | "attention" | "neutral") {
  el("review-title").textContent = title;
  el("review").setAttribute("data-phase", phase);
  el("review-icon").replaceChildren(icon(({ preview: "repository", running: "refresh", success: "review", attention: "warning", neutral: "repository" } as const)[phase]));
  el("review-remote").hidden = phase !== "preview";
  el("review-footnote").hidden = true;
  el("review-return").hidden = phase === "preview" || phase === "running";
}

async function openReview() {
  if (busy || cancelPending || !statusLoaded) return;
  clearLines();
  previousScroll = [window.scrollX, window.scrollY];
  reviewHeading("Outgoing sync", "preview");
  displayReview(true);
  el("review-items").replaceChildren();
  clearCommitEditor();
  reviewId = null;
  controls();
  const remote = el("remote").textContent;
  el("review-remote").textContent = remote === "Last fetched: 0 ahead · 0 behind" ? "Branches matched at last fetch" : remote;
  el("review-message").textContent = "Loading outgoing preview…";
  el("close-review").focus();
  const token = ++serial;
  busy = true;
  controls();
  try {
    const view = await invoke("open_review");
    if (token !== serial) {
      if (view.id) await invoke("discard_review", { id: view.id });
      return;
    }
    reviewId = view.id ?? null;
    massDeleteReview = view.outcome === "mass_delete";
    if (view.outcome === "review" && typeof reviewId === "string" && reviewId.length && typeof view.commit_message === "string" && view.commit_message.length) {
      suggestedMessage = view.commit_message;
      el("commit-message").value = suggestedMessage;
      el("commit-editor").hidden = false;
    }
    el("review-footnote").hidden = !reviewId;
    el("review-message").textContent = view.outcome === "review" && view.message === "Outgoing preview loaded."
      ? (view.items.length ? "" : "No outgoing changes to review.") : view.message;
    if (view.outcome === "terminal_recovery") {
      showRecovery(view.message);
      clearDecisions();
      entries = [];
      statusLoaded = false;
      clearMachineFiles();
      updateWorkspace({ status: "Repository status needs attention." });
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
  const order = ["Attention", "Refusals", "Held or undecided", "Changes", "Captures", "Repository changes", "Hypothetical effects", "Already-ahead commits", "Hooks", "Reached stages"];
  const labels = [...order.filter(label => groups.has(label)), ...[...groups.keys()].filter(label => !order.includes(label))];
  const titles = new Map([
    ["Reached stages", "Sync activity"], ["Changes", "File changes"], ["Hypothetical effects", "Planned changes"],
    ["Already-ahead commits", "Local commits"], ["Captures", "Capture from this computer"], ["Refusals", "Blocked changes"],
    ["Held or undecided", "Needs a decision"], ["Hooks", "Hook activity"],
  ]);
  for (const label of labels) {
    const items = groups.get(label);
    if (!items || (label === "Suggested message" && !el("commit-editor").hidden)) continue;
    const section = document.createElement("details");
    section.className = label === "Reached stages" ? "sync-activity" : "sync-group";
    section.open = ["Changes", "Attention", "Refusals", "Captures", "Held or undecided"].includes(label) || labels.length === 1;
    const heading = document.createElement("summary");
    heading.textContent = titles.get(label) ?? label;
    const count = document.createElement("span");
    count.className = "sync-count";
    count.textContent = String(items.length);
    heading.append(count, icon("chevron-right", "disclosure-glyph"));
    const list = document.createElement("ul");
    for (const item of items) {
      const row = document.createElement("li");
      const description = label === "Reached stages" ? stageNames.get(item.text) : label === "Hooks" ? hookNames.get(item.text) : undefined;
      const separator = label === "Changes" || label === "Hypothetical effects" ? item.text.lastIndexOf(" · ") : -1;
      if (separator >= 0) {
        const path = document.createElement("span");
        path.textContent = item.text.slice(0, separator);
        const effect = document.createElement("span");
        effect.className = "sync-effect";
        effect.textContent = item.text.slice(separator + 3);
        row.className = "sync-file";
        row.append(icon("changed"), path, effect);
      } else {
        row.textContent = description ?? item.text;
      }
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
    section.append(heading, list);
    el("review-items").append(section);
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

const stageNames = new Map([
  ["capture", "Capture local changes"], ["commit", "Commit repository changes"], ["pull", "Pull from remote"],
  ["apply", "Apply repository files"], ["reencrypt", "Update encryption"], ["hooks", "Check hooks"],
  ["push", "Push to remote"], ["blocked_by_repo_hold", "Stopped at repository hold"],
]);
const hookNames = new Map([["deferred", "Deferred for separate review"], ["disabled", "Disabled for this sync"]]);

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
  reviewHeading("Confirm outgoing sync", "running");
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
      if (label) {
        reviewHeading("Sync in progress", "running");
        el("review-message").textContent = label;
      }
    };
    const theme = document.documentElement.getAttribute("data-theme");
    const dark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
    const args: { id: string; progress: typeof progress; dark: boolean; commitMessage?: string; selectedLines?: string[]; massDelete?: boolean } = { id, progress, dark };
    if (commitMessage !== null) args.commitMessage = commitMessage;
    if (selected) args.selectedLines = selectedLines;
    if (massDelete) args.massDelete = true;
    const result = await invoke("run_reviewed_sync", args);
    acceptingProgress = false;
    if (writerCancel) await writerCancel;
    const titles = new Map([["done", "Sync complete"], ["declined", "Sync declined"], ["cancelled", "Sync cancelled"], ["error", "Sync failed"]]);
    const neutral = result.outcome === "declined" || result.outcome === "cancelled";
    reviewHeading(titles.get(result.outcome) ?? "Sync needs attention", result.outcome === "done" ? "success" : neutral ? "neutral" : "attention");
    el("review-message").textContent = result.outcome === "done" && result.message === "Sync completed."
      ? (result.items.some(item => item.group === "Changes") ? "" : "No file changes reported.") : result.message;
    renderReviewItems(result.items);
    if (result.outcome === "terminal_recovery") {
      el("review-message").textContent += " Open a terminal and follow dotman's recovery instructions before continuing.";
      showRecovery(result.message, result.recovery);
    } else if (result.outcome === "cleanup_unresolved") {
      el("review-message").textContent += " Cleanup is unresolved. Check dotman in a terminal before continuing.";
    } else {
      shouldRefresh = result.refresh || result.outcome === "declined";
    }
  } catch {
    acceptingProgress = false;
    if (writerCancel) await writerCancel;
    reviewHeading("Sync needs attention", "attention");
    el("review-message").textContent = "Sync request failed. Check dotman in a terminal before refreshing.";
  } finally {
    writerActive = false;
    writerKind = null;
    busy = false;
    clearCommitEditor();
    controls();
  }
  if (shouldRefresh) await refresh({ preserveReview: true });
}

async function applyDecisions() {
  if (busy || cancelPending || !statusLoaded || !decisions.size) return;
  const answers = Array.from(decisions, ([id, choice]) => ({ id, action: choice.action }));
  retryDecisions = false;
  decisionsOpen = true;
  el("decision-message").textContent = "Waiting for native confirmation…";
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
  displayReview(false);
  el("decision-results").replaceChildren();
  el("decision-message").focus();
  controls();
  let shouldRefresh = false;
  try {
    const result = await invoke("apply_reviewed_files", { answers });
    if (writerCancel) await writerCancel;
    retryDecisions = result.outcome === "declined" || result.outcome === "stale";
    el("decision-message").textContent = result.outcome === "declined"
      ? "Cancelled. No files changed."
      : result.message;
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
      getWorkspaceView().page !== "settings" || el("machine-files").hidden || !el("machine-files").open) return;
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
  el("setup-retry").hidden = true;
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
    const stopped = result.outcome === "declined" || result.outcome === "cancelled";
    el("setup-message").textContent = stopped ? result.message : `${result.outcome}: ${result.message}`;
    if (stopped) {
      updateWorkspace({ status: "Repository setup cancelled." });
      el("setup-retry").hidden = false;
    }
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

const discardDialog = el("discard-review-work") as HTMLDialogElement;
let afterDiscard: (() => void) | null = null;

function confirmReviewExit(refreshing: boolean, follow: () => void) {
  if (afterDiscard || (refreshing && (busy || cancelPending))) return;
  const pending: string[] = [];
  if (refreshing && decisions.size) {
    pending.push(`${decisions.size} queued file decision${decisions.size === 1 ? "" : "s"}`);
  }
  if (reviewId !== null) {
    const lines = Array.from(lineReceipts.values()).reduce((count, receipt) => count + receipt.selected.size, 0);
    if (lines) pending.push(`${lines} selected edit${lines === 1 ? "" : "s"}`);
    if (suggestedMessage !== null && el("commit-message").value !== defaultCommitMessage()) {
      pending.push("your edited commit message");
    }
  }
  if (!pending.length) { follow(); return; }
  afterDiscard = follow;
  el("discard-work-title").textContent = refreshing ? "Refresh and clear your review?" : "Discard this review?";
  const work = new Intl.ListFormat("en").format(pending);
  el("discard-work-description").textContent = refreshing
    ? `Refreshing status will clear ${work}.`
    : `Closing this preview will clear ${work}.`;
  el("discard-review-work-confirm").textContent = refreshing ? "Clear and refresh" : "Discard and close";
  discardDialog.showModal();
  el("keep-review-work").focus();
}

function finishDiscard(accepted: boolean) {
  const follow = afterDiscard;
  afterDiscard = null;
  discardDialog.close();
  if (accepted) follow?.();
  else if (!el("review").hidden && suggestedMessage !== null && el("commit-message").value !== defaultCommitMessage()) {
    if (el("commit-editor").open) el("commit-message").focus();
    else el("commit-editor").querySelector("summary")?.focus();
  }
}
el("keep-review-work").addEventListener("click", () => finishDiscard(false));
el("discard-review-work-confirm").addEventListener("click", () => finishDiscard(true));
discardDialog.addEventListener("cancel", event => { event.preventDefault(); finishDiscard(false); });
function requestCloseReview() {
  if (el("close-review").disabled) return;
  confirmReviewExit(false, () => void closeReview());
}

async function closeReview() {
  if (el("close-review").disabled) return;
  clearLines();
  const token = ++serial;
  const id = reviewId;
  reviewId = null;
  controls();
  displayReview(false);
  el("review-items").replaceChildren();
  clearCommitEditor();
  el("context").hidden = !el("paths").children.length;
  if (statusLoaded) {
    el("entries").hidden = false;
    el(busy ? "page-heading" : "nav-review").focus({ preventScroll: true });
    window.scrollTo(...previousScroll);
  } else {
    el("message").focus();
  }
  setPage("files");
  if (id !== null) {
    try {
      await invoke("discard_review", { id });
    } catch {
      if (token === serial) updateWorkspace({ status: "Could not discard preview. Refresh status before continuing." });
    }
  }
}

function setPage(next: Page) {
  updateWorkspace({ page: next });
  document.body.setAttribute("data-page", next);
  el("settings").hidden = next !== "settings";
  controls();
}

function navigate(next: Page) {
  if (getWorkspaceView().page === "review" && next !== "review") {
    confirmReviewExit(false, () => changePage(next));
  } else changePage(next);
}

function changePage(next: Page) {
  const { page } = getWorkspaceView();
  if (busy || cancelPending || next === page) return;
  decisionsOpen = false;
  if (page === "review") {
    if (diffOrigin?.review) {
      clearDiff();
      diffOrigin = null;
    }
    void closeReview();
  } else if (diffOrigin && next !== "review") {
    clearDiff();
    rowNodes.get(diffOrigin.id)?.setAttribute("aria-current", "false");
    diffOrigin = null;
  }
  setPage(next);
  el("page-heading").focus();
  if (next === "review") {
    if (statusLoaded) void openReview();
    else {
      reviewHeading("Outgoing sync", "preview");
      el("review-remote").textContent = "";
      displayReview(true);
      el("review-message").textContent = "Status is unavailable. Refresh status before opening an outgoing preview.";
      controls();
    }
  } else if (next === "files") {
    el("entries").hidden = !statusLoaded;
  }
}

const startTracking = trackingFlow(invoke, () => statusLoaded && !busy && !cancelPending && el("recovery").hidden === true,
  async working => {
    if (working) writerCancel = null;
    else if (writerCancel) await writerCancel;
    writerActive = working;
    writerKind = working ? "tracking" : null;
    busy = working;
    controls();
  },
  async (message, key) => {
    await refresh();
    if (key && statusLoaded) {
      folderMode = true;
      el("search").value = "";
      el("group-folders").setAttribute("aria-pressed", "true");
      el("group-status").setAttribute("aria-pressed", "false");
      renderRows();
      const row = entries.find(entry => entry.key === key);
      const node = row && rowNodes.get(row.id);
      if (node) { reveal(node); node.focus(); }
    }
    updateWorkspace({status: message});
  }, () => selectedFolder);

el("rows").addEventListener("focusin", event => {
  const row = event.target instanceof Element ? event.target.closest("[role=treeitem]") : null;
  selectTreeFolder(row?.getAttribute("data-folder-path") ?? null);
});

for (const name of ["files", "review", "settings"] as const) {
  el(`nav-${name}`).addEventListener("click", () => navigate(name));
}
el("refresh").addEventListener("click", () => confirmReviewExit(true, () => {
  dismissDecisionResult();
  if (getWorkspaceView().page === "review") {
    void closeReview();
    el("message").focus();
  }
  void refresh();
}));
for (const [id, folder, feedback] of [
  ["open-installation", "installation", "installation-feedback"],
  ["open-repository", "repository", "repository-folder-feedback"],
  ["open-recovery-repository", "repository", "recovery-folder-feedback"],
] as const) {
  el(id).addEventListener("click", async () => {
    if (busy || cancelPending || !el(id).getClientRects().length) return;
    const token = serial;
    el(feedback).textContent = "";
    try {
      await invoke("open_workspace_folder", { folder });
    } catch {
      if (token === serial) el(feedback).textContent = "Could not open this folder. Check that the folder exists and a file manager is installed.";
    }
  });
}
for (const id of ["retry-status", "setup-check", "setup-retry", "recovery-check"] as const) {
  el(id).addEventListener("click", async () => {
    if (busy || cancelPending) return;
    const button = el(id);
    const hadFocus = document.activeElement === button;
    if (hadFocus) el("message").focus();
    await refresh();
    if (id === "setup-retry" && el("setup-retry").hidden) el("setup-result").hidden = true;
    if (!hadFocus || document.activeElement !== el("message")) return;
    if (!el("recovery").hidden) el("recovery-check").focus();
    else if (!el("setup").hidden) el("setup-check").focus();
    else if (!el("status-issue").hidden) el("retry-status").focus();
    else if (id === "setup-retry" && !button.hidden) button.focus();
    else if (statusLoaded && getWorkspaceView().page === "files") el("search").focus();
    else if (statusLoaded) el("page-heading").focus();
  });
}
el("cancel").addEventListener("click", cancel);
el("cancel-review").addEventListener("click", cancel);
el("close-diff").addEventListener("click", closeDiff);
el("replacement-batch").addEventListener("click", () => {
  if (busy || cancelPending || !replacementReview || !statusLoaded ||
      replacementReview.token !== serial || !entries.includes(replacementReview.entry) ||
      diffOrigin?.id !== replacementReview.entry.id || diffOrigin.review || el("diff").hidden) return;
  const { entry, launcher } = replacementReview;
  replacementReview = null;
  el("replacement-review").hidden = true;
  decisions.set(entry.id, { action: "take_repo", key: entry.key });
  renderDecisions();
  launcher.focus({ preventScroll: true });
  const body = el("comparison-body");
  if (body.clientHeight > 0) {
    body.scrollTop += el("diff-text").getBoundingClientRect().top - body.getBoundingClientRect().top;
  }
});
el("replacement-back").addEventListener("click", () => {
  if (busy || cancelPending || !replacementReview) return;
  const { launcher } = replacementReview;
  replacementReview = null;
  el("replacement-review").hidden = true;
  launcher.focus();
});
el("more-comparisons").addEventListener("click", () => appendComparisons(true));
el("clear-search").addEventListener("click", clearSearch);
el("search").addEventListener("keydown", event => {
  if (event.key === "Escape" && el("search").value && !event.isComposing) {
    event.preventDefault();
    clearSearch();
  }
});
el("search").addEventListener("input", () => renderRows());
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
  if (!entry) return;
  const fromOverview = diffOrigin.row && comparisonRows.get(diffOrigin.id)?.button === diffOrigin.row;
  void openDiff(entry, (fromOverview && comparisonRows.get(entry.id)?.button) || rowNodes.get(entry.id));
});
el("close-review").addEventListener("click", requestCloseReview);
el("review-return").addEventListener("click", requestCloseReview);
el("sync-reviewed").addEventListener("click", () => syncReviewed());
el("sync-mass-delete").addEventListener("click", () => syncReviewed(false, true));
el("sync-lines").addEventListener("click", () => syncReviewed(true));
el("leave-lines").addEventListener("click", () => { if (!busy && !cancelPending) clearLines(); });
el("restore-commit-message").addEventListener("click", () => {
  const message = defaultCommitMessage();
  if (message === null || busy || cancelPending) return;
  el("commit-message").value = message;
  updateCommitValidation();
  el("commit-message").focus();
});
el("commit-message").addEventListener("input", updateCommitValidation);
el("clear-decisions").addEventListener("click", clearDecisions);
el("apply-decisions").addEventListener("click", applyDecisions);
el("review-decisions").addEventListener("click", () => {
  if (busy || cancelPending) return;
  decisionsOpen = true;
  controls();
  el("decisions-title").focus({ preventScroll: true });
});
function closeDecisions(restoreFocus = true) {
  if (busy || cancelPending) return;
  decisionsOpen = false;
  controls();
  if (!restoreFocus) return;
  if (getWorkspaceView().queued === null) el("search").focus();
  else window.requestAnimationFrame(() => el("review-decisions").focus({ preventScroll: true }));
}
el("open-destination").addEventListener("click", () => {
  if (el("open-destination").disabled) return;
  navigate("settings");
  window.requestAnimationFrame(() => {
    const section = document.querySelector<HTMLDetailsElement>("#destination-settings");
    if (!section) return;
    section.open = true;
    section.querySelector("summary")?.focus();
  });
});
el("dismiss-held").addEventListener("click", () => {
  if (writerKind === "held") return;
  el("held-result").hidden = true;
  el("search").focus();
});
el("close-decisions").addEventListener("click", () => closeDecisions());
function dismissDecisionResult() {
  retryDecisions = false;
  decisionsOpen = false;
  el("decision-message").textContent = "";
  el("decision-results").replaceChildren();
}
el("retry-decisions").addEventListener("click", async () => {
  if (busy || cancelPending) return;
  dismissDecisionResult();
  el("message").focus({ preventScroll: true });
  await refresh();
  if (statusLoaded) el("search").focus({ preventScroll: true });
});
window.addEventListener("keydown", event => {
  if (event.defaultPrevented || discardDialog.open || (el("tracking-drawer") as HTMLDialogElement).open || event.altKey || event.metaKey || event.shiftKey) return;
  const { page } = getWorkspaceView();
  if (event.ctrlKey && event.key.toLowerCase() === "f" && page === "files" && statusLoaded && !busy && !cancelPending) {
    event.preventDefault();
    if (decisionsOpen) closeDecisions(false);
    if (!el("search").getClientRects().length && !el("diff").hidden) closeDiff();
    el("search").focus();
    el("search").select();
  } else if (!event.ctrlKey && event.key === "Escape" && readingDiff && !cancelPending) {
    closeDiff();
    event.preventDefault();
  } else if (!event.ctrlKey && event.key === "Escape" && !busy && !cancelPending) {
    if (!el("review").hidden) requestCloseReview();
    else if (decisionsOpen && !el("decisions").hidden) closeDecisions();
    else if (replacementReview) el("replacement-back").click();
    else if (!el("diff").hidden) closeDiff();
    else if (page === "review") requestCloseReview();
    else return;
    event.preventDefault();
  }
});
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
  canStart: () => !busy && !cancelPending && statusLoaded && getWorkspaceView().page === "settings",
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
export async function destinationOperation(change: DestinationChange | null, expected: string | null) {
  if (busy || cancelPending || !statusLoaded || getWorkspaceView().page !== "settings") {
    throw new Error("Refresh repository status before reviewing the destination.");
  }
  if (change && decisions.size) throw new Error("Finish or clear your queued file decisions before changing origin.");
  busy = true;
  writerActive = Boolean(change);
  writerKind = change ? "destination" : null;
  writerCancel = null;
  controls();
  const theme = document.documentElement.getAttribute("data-theme");
  let destination: Destination;
  try {
    destination = await invoke("sync_destination", {change, expected, dark: theme === "dark" ||
      (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches)});
  } finally {
    if (writerCancel) await writerCancel;
    writerActive = false;
    writerKind = null;
    busy = false;
    controls();
  }
  if (change) await refresh({ keepFilesVisible: true });
  return destination;
}

let windowWasAway = false;
window.addEventListener("blur", () => { windowWasAway = true; });
window.addEventListener("focus", () => {
  if (!windowWasAway) return;
  windowWasAway = false;
  if (busy || cancelPending || !statusLoaded || getWorkspaceView().page !== "files") return;
  if (diffOrigin || decisions.size || decisionsOpen || el("decision-message").textContent
    || document.querySelector("dialog[open], #track-menu[open], .held-row[open]")) return;
  void refresh({ keepFilesVisible: true });
});
void refresh();
