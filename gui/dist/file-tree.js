const states = new WeakMap();
function icon(path) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    const shape = document.createElementNS("http://www.w3.org/2000/svg", "path");
    shape.setAttribute("d", path);
    svg.append(shape);
    return svg;
}
export function render(root, entries, query, makeLeaf) {
    const previous = states.get(root);
    const state = { children: new Map(), expansion: { closed: false } };
    states.set(root, state);
    const needle = query.toLocaleLowerCase();
    const tree = { kind: "folder", name: "", children: new Map(), items: [], state, count: 0 };
    let count = 0;
    for (const entry of entries) {
        const matched = entry.key.toLocaleLowerCase().includes(needle) || entry.status.toLocaleLowerCase().includes(needle);
        const parts = entry.key ? entry.key.split("/") : [entry.status];
        let node = tree;
        let saved = state;
        let old = previous;
        for (const part of parts.slice(0, -1)) {
            const oldChild = old?.children.get(part);
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
    if (!count)
        return 0;
    const list = document.createElement("ul");
    list.className = "file-tree";
    root.append(list);
    const stack = [{ node: tree, list, parentSummary: null }];
    while (stack.length) {
        const current = stack.pop();
        if (!current)
            break;
        for (const item of current.node.items) {
            if (item.kind === "leaf") {
                const leaf = makeLeaf(item.entry, item.name);
                const control = leaf.querySelector("button, summary");
                if (control) {
                    const path = item.entry.key || item.name;
                    const context = item.entry.group === "held" ? " · details"
                        : `${item.entry.system ? " · system (read only)" : ""}${item.entry.supported ? "" : " · diff unavailable"}`;
                    control.setAttribute("aria-label", `${path}${item.entry.status ? ` — ${item.entry.status}` : ""}${context}`);
                }
                current.list.append(leaf);
                continue;
            }
            const li = document.createElement("li");
            li.className = "file-tree-folder";
            const details = document.createElement("details");
            details.open = Boolean(needle) || !item.state.expansion.closed;
            const summary = document.createElement("summary");
            const name = document.createElement("span");
            name.className = "file-tree-name";
            const names = [item.name || "(empty)"];
            let end = item;
            while (end.items.length === 1) {
                const child = end.items[0];
                if (!child || child.kind !== "folder" || child.state.expansion.closed)
                    break;
                names.push(child.name || "(empty)");
                end = child;
            }
            name.textContent = names.join("/");
            name.prepend(icon("M1.75 4.25h4l1.35 1.5h7.15v6.75H1.75z"));
            const total = document.createElement("span");
            total.className = "file-tree-count";
            total.textContent = String(item.count);
            summary.append(name, total);
            const children = document.createElement("ul");
            details.append(summary, children);
            li.append(details);
            current.list.append(li);
            details.addEventListener("toggle", () => {
                if (!needle)
                    item.state.expansion.closed = !details.open;
            });
            summary.addEventListener("keydown", event => {
                if (event.key === "ArrowRight" && !details.open) {
                    details.open = true;
                    event.preventDefault();
                }
                else if (event.key === "ArrowLeft") {
                    if (details.open)
                        details.open = false;
                    else if (current.parentSummary)
                        current.parentSummary.focus();
                    else
                        return;
                    event.preventDefault();
                }
            });
            stack.push({ node: end, list: children, parentSummary: summary });
        }
    }
    return count;
}
