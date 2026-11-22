import { icon } from "./fluent-icons.js";

export interface TreeEntry {
  key: string;
  status: string;
  group?: string;
  system?: boolean;
  supported?: boolean;
}

interface FolderState {
  children: Map<string, FolderState>;
  expansion: { closed: boolean };
}

interface Folder<T> {
  kind: "folder";
  name: string;
  children: Map<string, Folder<T>>;
  items: Item<T>[];
  state: FolderState;
  count: number;
}

type Item<T> = Folder<T> | { kind: "leaf"; entry: T; name: string };

const states = new WeakMap<HTMLElement, FolderState>();
const expansions = new WeakMap<HTMLElement, (open: boolean) => void>();

export function reveal(row: HTMLElement) {
  for (let parent = row.parentElement; parent; parent = parent.parentElement) {
    expansions.get(parent)?.(true);
  }
}

function treeKeyboard(list: HTMLUListElement, first: HTMLElement) {
  let tabStop = first;
  let prefix = "";
  let lastKeyAt = 0;
  tabStop.setAttribute("tabindex", "0");
  list.addEventListener("focusin", event => {
    if (!(event.target instanceof HTMLElement)) return;
    const item = event.target.closest<HTMLElement>("[role=treeitem]");
    if (!item || item === tabStop) return;
    tabStop.setAttribute("tabindex", "-1");
    tabStop = item;
    tabStop.setAttribute("tabindex", "0");
  });
  list.addEventListener("keydown", event => {
    const target = event.target;
    if (event.defaultPrevented || event.ctrlKey || event.altKey || event.metaKey ||
        !(target instanceof HTMLElement) || !target.matches("[role=treeitem]")) return;
    const expand = expansions.get(target);
    const expanded = target.getAttribute("aria-expanded") === "true";
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      if (expand) expand(!expanded);
      else target.click();
      return;
    }
    if (expand && ((event.key === "ArrowLeft" && expanded) || (event.key === "ArrowRight" && !expanded))) {
      event.preventDefault();
      expand(!expanded);
      return;
    }
    const visible = Array.from(list.querySelectorAll<HTMLElement>("[role=treeitem]"))
      .filter(control => control.getClientRects().length > 0);
    const index = visible.indexOf(target);
    if (index < 0) return;
    let next: HTMLElement | undefined;
    switch (event.key) {
      case "Home": next = visible[0]; break;
      case "End": next = visible.at(-1); break;
      case "ArrowUp": next = visible[index - 1]; break;
      case "ArrowDown": next = visible[index + 1]; break;
      case "ArrowRight":
        if (expanded && target.classList.contains("file-tree-folder")) {
          next = visible[index + 1];
        }
        break;
      case "ArrowLeft":
        next = target.parentElement?.closest<HTMLElement>("[role=treeitem]") ?? undefined;
        break;
      default: {
        if (event.key.length !== 1 || event.key === " ") return;
        const key = event.key.toLocaleLowerCase();
        const now = performance.now();
        prefix = now - lastKeyAt > 650 || (prefix.length === 1 && prefix === key) ? key : prefix + key;
        lastKeyAt = now;
        const matches = (control: HTMLElement) => (control.getAttribute("data-tree-label") ?? "")
          .toLocaleLowerCase().startsWith(prefix);
        next = visible.slice(index + 1).find(matches) ?? visible.slice(0, index + 1).find(matches);
        break;
      }
    }
    if (!next) return;
    event.preventDefault();
    next.focus();
  });
}

export function render<T extends TreeEntry>(
  root: HTMLElement,
  entries: readonly T[],
  query: string,
  makeLeaf: (entry: T, displayName: string) => HTMLLIElement,
): number {
  const previous = states.get(root);
  const state: FolderState = { children: new Map(), expansion: { closed: false } };
  states.set(root, state);
  const needle = query.toLocaleLowerCase();
  const tree: Folder<T> = { kind: "folder", name: "", children: new Map(), items: [], state, count: 0 };
  let count = 0;

  for (const entry of entries) {
    const matched = entry.key.toLocaleLowerCase().includes(needle) || entry.status.toLocaleLowerCase().includes(needle);
    const parts = entry.key ? entry.key.split("/") : [entry.status];
    let node = tree;
    let saved = state;
    let old = previous;
    for (const part of parts.slice(0, -1)) {
      const oldChild: FolderState | undefined = old?.children.get(part);
      let savedChild = saved.children.get(part);
      if (!savedChild) {
        savedChild = { children: new Map(), expansion: oldChild?.expansion ?? { closed: false } };
        saved.children.set(part, savedChild);
      }
      if (matched) {
        let child = node.children.get(part);
        if (!child) {
          child = { kind: "folder", name: part, children: new Map(), items: [], state: savedChild, count: 0 };
          node.children.set(part, child);
          node.items.push(child);
        }
        child.count++;
        node = child;
      }
      saved = savedChild;
      old = oldChild;
    }
    if (matched) {
      count++;
      node.items.push({ kind: "leaf", entry, name: parts[parts.length - 1] ?? "" });
    }
  }

  root.replaceChildren();
  if (!count) return 0;
  const list = document.createElement("ul");
  list.className = "file-tree";
  list.setAttribute("role", "tree");
  list.setAttribute("aria-label", "All files");
  root.append(list);
  let first: HTMLElement | undefined;
  const stack = [{ node: tree, list }];
  while (stack.length) {
    const current = stack.pop();
    if (!current) break;
    for (const item of current.node.items) {
      if (item.kind === "leaf") {
        const leaf = makeLeaf(item.entry, item.name);
        first ??= leaf;
        leaf.setAttribute("role", "treeitem");
        leaf.setAttribute("tabindex", "-1");
        leaf.setAttribute("data-tree-label", item.name);
        if (!leaf.getAttribute("aria-label")) {
          const path = item.entry.key || item.name;
          const context = item.entry.group === "held" ? " · details"
            : `${item.entry.system ? " · system (read only)" : ""}${item.entry.supported ? "" : " · diff unavailable"}`;
          leaf.setAttribute("aria-label", `${path}${item.entry.status ? ` — ${item.entry.status}` : ""}${context}`);
        }
        const details = leaf.querySelector<HTMLDetailsElement>("details");
        if (details) {
          const syncExpanded = () => leaf.setAttribute("aria-expanded", String(details.open));
          const summary = details.querySelector("summary");
          summary?.setAttribute("tabindex", "-1");
          summary?.addEventListener("click", () => leaf.focus());
          details.addEventListener("toggle", syncExpanded);
          expansions.set(leaf, open => {
            details.open = open;
            syncExpanded();
          });
          syncExpanded();
        }
        current.list.append(leaf);
        continue;
      }
      const li = document.createElement("li");
      li.className = "file-tree-folder";
      li.setAttribute("role", "treeitem");
      li.setAttribute("tabindex", "-1");
      first ??= li;
      const row = document.createElement("div");
      row.className = "file-tree-branch";
      const name = document.createElement("span");
      name.className = "file-tree-name";
      const names = [item.name || "(empty)"];
      let end = item;
      while (end.items.length === 1) {
        const child = end.items[0];
        if (!child || child.kind !== "folder" || child.state.expansion.closed) break;
        names.push(child.name || "(empty)");
        end = child;
      }
      const label = names.join("/");
      name.textContent = label;
      li.setAttribute("data-tree-label", label);
      li.setAttribute("aria-label", `${label} (${item.count})`);
      name.prepend(icon("folder"));
      name.prepend(icon("chevron-right", "disclosure-glyph"));
      const total = document.createElement("span");
      total.className = "file-tree-count";
      total.textContent = String(item.count);
      row.append(name, total);
      const children = document.createElement("ul");
      children.setAttribute("role", "group");
      li.append(row, children);
      current.list.append(li);
      const expand = (open: boolean) => {
        li.setAttribute("aria-expanded", String(open));
        children.hidden = !open;
        if (!needle) item.state.expansion.closed = !open;
      };
      expansions.set(li, expand);
      expand(Boolean(needle) || !item.state.expansion.closed);
      li.addEventListener("click", event => {
        if (event.target !== li && !(event.target instanceof Node && row.contains(event.target))) return;
        li.focus();
        expand(children.hidden === true);
      });
      stack.push({ node: end, list: children });
    }
  }
  if (first) treeKeyboard(list, first);
  return count;
}
