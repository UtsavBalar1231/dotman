import { icon } from "./fluent-icons.js";
import type { IconName } from "./fluent-icons.js";

type Side = "Home" | "System";
type Line = { type: string; text: string; old: number | null; next: number | null };
type Hunk = { header: string; lines: Line[] };
type Patch = { header: string[]; hunks: Hunk[] };

function required(id: string): HTMLElement {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing ${id}`);
  return element;
}

const pre = required("diff-text");
const tools = required("reader-tools");
let changes: HTMLElement[] = [];
let current = -1;
let previous: HTMLButtonElement;
let next: HTMLButtonElement;
let position: HTMLElement;
let copyLabels: HTMLElement | undefined;
let anchorTop = 0;

function node<K extends keyof HTMLElementTagNameMap>(tag: K, className: string, text?: string): HTMLElementTagNameMap[K] {
  const result = document.createElement(tag);
  result.className = className;
  if (text !== undefined) result.textContent = text;
  return result;
}

function heading(label: string, className: string, name: IconName): HTMLElement {
  const result = node("span", className, label);
  result.prepend(icon(name));
  return result;
}

function parse(text: string): Patch | null {
  if (text.length > 500000 || !text.endsWith("\n") || /[\x00-\x08\x0b-\x1f\x7f]/.test(text)) return null;
  const lines = text.slice(0, -1).split("\n");
  if (lines.length > 1200 || !lines[0].startsWith("diff --git ")) return null;
  let i = 1;
  while (i < lines.length && /^(index [0-9a-f]+\.\.[0-9a-f]+(?: [0-7]+)?|(?:old mode|new mode|new file mode|deleted file mode) [0-7]+)$/.test(lines[i])) i++;
  if (!lines[i]?.startsWith("--- ") || !lines[i + 1]?.startsWith("+++ ")) return null;
  const oldPath = lines[i].slice(4);
  const newPath = lines[i + 1].slice(4);
  if (!(oldPath.startsWith("repo/") || oldPath === "/dev/null") ||
      !(newPath.startsWith("home/") || newPath === "/dev/null") ||
      (oldPath !== "/dev/null" && newPath !== "/dev/null" && oldPath.slice(5) !== newPath.slice(5)) ||
      (oldPath === "/dev/null" && newPath === "/dev/null")) return null;
  if (/\s/.test(oldPath) || /\s/.test(newPath) ||
      lines[0] !== `diff --git ${oldPath === "/dev/null" ? newPath : oldPath} ${newPath === "/dev/null" ? oldPath : newPath}`) return null;
  const header = lines.slice(0, i + 2);
  i += 2;
  const hunks: Hunk[] = [];
  let previousOld = 0;
  let previousNew = 0;
  while (i < lines.length) {
    const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*)?$/.exec(lines[i]);
    if (!match) return null;
    const [oldStart, oldCount, newStart, newCount] = [match[1], match[2] ?? "1", match[3], match[4] ?? "1"].map(Number);
    if (![oldStart, oldCount, newStart, newCount].every(Number.isSafeInteger) ||
        (oldCount && !oldStart) || (newCount && !newStart) ||
        oldStart < previousOld || newStart < previousNew ||
        !Number.isSafeInteger(oldStart + oldCount) || !Number.isSafeInteger(newStart + newCount)) return null;
    const hunk: Hunk = { header: lines[i++], lines: [] };
    let old = oldStart;
    let nextLine = newStart;
    let usedOld = 0;
    let usedNew = 0;
    let changed = false;
    let adding = false;
    while (i < lines.length && !lines[i].startsWith("@@ ")) {
      const line = lines[i++];
      const type = line[0];
      if (![" ", "-", "+"].includes(type)) return null;
      if (type === "-" && adding) return null;
      adding = type === "+" || (type !== " " && adding);
      if (type !== "+") usedOld++;
      if (type !== "-") usedNew++;
      if (usedOld > oldCount || usedNew > newCount) return null;
      hunk.lines.push({ type, text: line.slice(1), old: type === "+" ? null : old++, next: type === "-" ? null : nextLine++ });
      changed ||= type !== " ";
    }
    if (usedOld !== oldCount || usedNew !== newCount || !changed) return null;
    previousOld = oldStart + oldCount;
    previousNew = newStart + newCount;
    hunks.push(hunk);
  }
  return hunks.length ? { header, hunks } : null;
}

function highlight(a: string, b: string): [number, number] | null {
  if (a.length > 2000 || b.length > 2000) return null;
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let end = 0;
  while (end < a.length - start && end < b.length - start && a[a.length - 1 - end] === b[b.length - 1 - end]) end++;
  if (!start && !end) return null;
  if (!/\S/.test(a.slice(start, a.length - end)) && !/\S/.test(b.slice(start, b.length - end))) return null;
  return [start, end];
}

function cell(line: Line | null, side: "old" | "new", mark: [number, number] | null): HTMLElement {
  const result = node("span", `dr-cell dr-${line ? (line.type === "-" ? "del" : line.type === "+" ? "add" : "context") : "empty"} dr-${side}`);
  const number = node("span", "dr-number", line ? String(side === "old" ? (line.old ?? "") : (line.next ?? "")) : "");
  const symbol = node("span", "dr-symbol", line?.type === "-" ? "-" : line?.type === "+" ? "+" : " ");
  const code = node("span", "dr-code");
  if (line && mark) {
    const [start, end] = mark;
    code.append(node("span", "", line.text.slice(0, start)),
      node("span", "dr-token", line.text.slice(start, line.text.length - end)),
      node("span", "", end ? line.text.slice(-end) : ""));
  } else if (line) code.textContent = line.text;
  result.append(number, symbol, code);
  return result;
}

function row(old: Line | null, nextLine: Line | null): HTMLElement {
  const result = node("span", `dr-row${old?.type === " " ? " dr-context-row" : ""}`);
  const mark = old && nextLine && old.type === "-" && nextLine.type === "+" ? highlight(old.text, nextLine.text) : null;
  result.append(cell(old, "old", mark), cell(nextLine, "new", mark));
  return result;
}

function render(patch: Patch, sideLabel: Side): HTMLElement {
  const fragment = document.createDocumentFragment();
  const header = node("span", "dr-header", patch.header.join("\n"));
  header.hidden = true;
  fragment.append(header);
  const headings = node("span", "dr-headings");
  copyLabels = headings;
  headings.append(
    heading("Repository", "dr-repository", "repository"),
    heading(sideLabel === "Home" ? "This computer" : "System", `dr-${sideLabel.toLowerCase()}`, sideLabel === "Home" ? "home" : "desktop"),
  );
  fragment.append(headings);
  for (const hunk of patch.hunks) {
    fragment.append(node("span", "dr-hunk", hunk.header));
    for (let i = 0; i < hunk.lines.length;) {
      if (hunk.lines[i].type === " ") {
        const line = hunk.lines[i++];
        fragment.append(row(line, line));
        continue;
      }
      const removed: Line[] = [];
      const added: Line[] = [];
      while (i < hunk.lines.length && hunk.lines[i].type !== " ") {
        const line = hunk.lines[i++];
        (line.type === "-" ? removed : added).push(line);
      }
      const group = node("span", "dr-change");
      changes.push(group);
      for (let n = 0; n < Math.max(removed.length, added.length); n++) {
        group.append(row(removed[n] ?? null, added[n] ?? null));
      }
      fragment.append(group);
    }
  }
  pre.replaceChildren(fragment);
  return header;
}

function button(label: string, action: () => void): HTMLButtonElement {
  const result = node("button", "", label);
  result.type = "button";
  result.addEventListener("click", action);
  return result;
}

function changeButton(label: string, step: -1 | 1): HTMLButtonElement {
  const result = button("", () => navigate(step));
  result.className = "icon-button";
  result.title = label;
  result.setAttribute("aria-label", label);
  result.append(icon(step === -1 ? "chevron-up" : "chevron-down"));
  return result;
}

function contentTop(): number {
  return pre.getBoundingClientRect().top + (copyLabels?.offsetHeight ?? 0);
}

function updateNavigation(): void {
  previous.disabled = current <= 0;
  next.disabled = current >= changes.length - 1;
  position.textContent = `Change ${current + 1} of ${changes.length}`;
  anchorTop = changes[current].getBoundingClientRect().top - contentTop();
}

function navigate(step: number): void {
  current = Math.max(0, Math.min(changes.length - 1, current + step));
  pre.scrollTop += changes[current].getBoundingClientRect().top - contentTop();
  updateNavigation();
}

pre.addEventListener("scroll", () => {
  if (!changes.length) return;
  const top = contentTop();
  let index = 0;
  if (pre.scrollHeight > pre.clientHeight && pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 2) {
    index = changes.length - 1;
  } else {
    for (let i = 1; i < changes.length; i++) {
      if (changes[i].getBoundingClientRect().top > top + 2) break;
      index = i;
    }
  }
  if (index !== current) {
    current = index;
    updateNavigation();
  } else {
    anchorTop = changes[current].getBoundingClientRect().top - top;
  }
});

window.addEventListener("resize", () => {
  if (!changes.length) return;
  pre.scrollTop += changes[current].getBoundingClientRect().top - contentTop() - anchorTop;
});

export function clear(): void {
  changes = [];
  copyLabels = undefined;
  current = -1;
  tools.hidden = true;
  tools.replaceChildren();
  pre.className = "";
  pre.textContent = "";
}

export function show(text: string, sideLabel: Side = "Home"): void {
  clear();
  const patch = parse(text);
  if (!patch) {
    pre.textContent = text;
    return;
  }
  pre.className = "diff-reader";
  const header = render(patch, sideLabel);
  current = 0;
  const toggle = button("Unified view", () => {
    const top = changes[current].getBoundingClientRect().top;
    const unified = pre.classList.toggle("dr-unified");
    toggle.textContent = unified ? "Split view" : "Unified view";
    toggle.setAttribute("aria-pressed", String(unified));
    pre.scrollTop += changes[current].getBoundingClientRect().top - top;
  });
  toggle.className = "dr-toggle";
  toggle.setAttribute("aria-pressed", "false");
  const metadata = button("Show metadata", () => {
    const top = changes[current].getBoundingClientRect().top;
    header.hidden = !header.hidden;
    metadata.textContent = header.hidden ? "Show metadata" : "Hide metadata";
    metadata.setAttribute("aria-expanded", String(!header.hidden));
    pre.scrollTop += changes[current].getBoundingClientRect().top - top;
  });
  metadata.setAttribute("aria-expanded", "false");
  previous = changeButton("Previous change", -1);
  next = changeButton("Next change", 1);
  position = node("span", "dr-position");
  position.setAttribute("aria-live", "polite");
  updateNavigation();
  const navigation = node("span", "dr-navigation");
  navigation.append(position, previous, next);
  navigation.hidden = changes.length === 1;
  const options = node("details", "dr-options");
  const summary = node("summary", "", "View options");
  summary.prepend(icon("chevron-right", "disclosure-glyph"));
  const optionsList = node("div", "dr-options-list");
  optionsList.append(toggle, metadata);
  options.append(summary, optionsList);
  options.addEventListener("keydown", event => {
    if (event.key !== "Escape" || !options.open) return;
    event.preventDefault();
    options.open = false;
    summary.focus();
  });
  tools.append(options, navigation);
  tools.hidden = false;
}
