function required(id) {
    const element = document.getElementById(id);
    if (!element)
        throw new Error(`Missing ${id}`);
    return element;
}
const pre = required("diff-text");
const tools = required("reader-tools");
let changes = [];
let current = -1;
let previous;
let next;
let position;
let anchorTop = 0;
function node(tag, className, text) {
    const result = document.createElement(tag);
    result.className = className;
    if (text !== undefined)
        result.textContent = text;
    return result;
}
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
function heading(label, className, path) {
    const result = node("span", className, label);
    result.prepend(icon(path));
    return result;
}
function parse(text) {
    if (text.length > 500000 || !text.endsWith("\n") || /[\x00-\x08\x0b-\x1f\x7f]/.test(text))
        return null;
    const lines = text.slice(0, -1).split("\n");
    if (lines.length > 1200 || !lines[0].startsWith("diff --git "))
        return null;
    let i = 1;
    while (i < lines.length && /^(index [0-9a-f]+\.\.[0-9a-f]+(?: [0-7]+)?|(?:old mode|new mode|new file mode|deleted file mode) [0-7]+)$/.test(lines[i]))
        i++;
    if (!lines[i]?.startsWith("--- ") || !lines[i + 1]?.startsWith("+++ "))
        return null;
    const oldPath = lines[i].slice(4);
    const newPath = lines[i + 1].slice(4);
    if (!(oldPath.startsWith("repo/") || oldPath === "/dev/null") ||
        !(newPath.startsWith("home/") || newPath === "/dev/null") ||
        (oldPath !== "/dev/null" && newPath !== "/dev/null" && oldPath.slice(5) !== newPath.slice(5)) ||
        (oldPath === "/dev/null" && newPath === "/dev/null"))
        return null;
    if (/\s/.test(oldPath) || /\s/.test(newPath) ||
        lines[0] !== `diff --git ${oldPath === "/dev/null" ? newPath : oldPath} ${newPath === "/dev/null" ? oldPath : newPath}`)
        return null;
    const header = lines.slice(0, i + 2);
    i += 2;
    const hunks = [];
    let previousOld = 0;
    let previousNew = 0;
    while (i < lines.length) {
        const match = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*)?$/.exec(lines[i]);
        if (!match)
            return null;
        const [oldStart, oldCount, newStart, newCount] = [match[1], match[2] ?? "1", match[3], match[4] ?? "1"].map(Number);
        if (![oldStart, oldCount, newStart, newCount].every(Number.isSafeInteger) ||
            (oldCount && !oldStart) || (newCount && !newStart) ||
            oldStart < previousOld || newStart < previousNew ||
            !Number.isSafeInteger(oldStart + oldCount) || !Number.isSafeInteger(newStart + newCount))
            return null;
        const hunk = { header: lines[i++], lines: [] };
        let old = oldStart;
        let nextLine = newStart;
        let usedOld = 0;
        let usedNew = 0;
        let changed = false;
        let adding = false;
        while (i < lines.length && !lines[i].startsWith("@@ ")) {
            const line = lines[i++];
            const type = line[0];
            if (![" ", "-", "+"].includes(type))
                return null;
            if (type === "-" && adding)
                return null;
            adding = type === "+" || (type !== " " && adding);
            if (type !== "+")
                usedOld++;
            if (type !== "-")
                usedNew++;
            if (usedOld > oldCount || usedNew > newCount)
                return null;
            hunk.lines.push({ type, text: line.slice(1), old: type === "+" ? null : old++, next: type === "-" ? null : nextLine++ });
            changed ||= type !== " ";
        }
        if (usedOld !== oldCount || usedNew !== newCount || !changed)
            return null;
        previousOld = oldStart + oldCount;
        previousNew = newStart + newCount;
        hunks.push(hunk);
    }
    return hunks.length ? { header, hunks } : null;
}
function highlight(a, b) {
    if (a.length > 2000 || b.length > 2000)
        return null;
    let start = 0;
    while (start < a.length && start < b.length && a[start] === b[start])
        start++;
    let end = 0;
    while (end < a.length - start && end < b.length - start && a[a.length - 1 - end] === b[b.length - 1 - end])
        end++;
    if (!start && !end)
        return null;
    if (!/\S/.test(a.slice(start, a.length - end)) && !/\S/.test(b.slice(start, b.length - end)))
        return null;
    return [start, end];
}
function cell(line, side, mark) {
    const result = node("span", `dr-cell dr-${line ? (line.type === "-" ? "del" : line.type === "+" ? "add" : "context") : "empty"} dr-${side}`);
    const number = node("span", "dr-number", line ? String(side === "old" ? (line.old ?? "") : (line.next ?? "")) : "");
    const symbol = node("span", "dr-symbol", line?.type === "-" ? "-" : line?.type === "+" ? "+" : " ");
    const code = node("span", "dr-code");
    if (line && mark) {
        const [start, end] = mark;
        code.append(node("span", "", line.text.slice(0, start)), node("span", "dr-token", line.text.slice(start, line.text.length - end)), node("span", "", end ? line.text.slice(-end) : ""));
    }
    else if (line)
        code.textContent = line.text;
    result.append(number, symbol, code);
    return result;
}
function row(old, nextLine) {
    const result = node("span", `dr-row${old?.type === " " ? " dr-context-row" : ""}`);
    const mark = old && nextLine && old.type === "-" && nextLine.type === "+" ? highlight(old.text, nextLine.text) : null;
    result.append(cell(old, "old", mark), cell(nextLine, "new", mark));
    return result;
}
function render(patch, sideLabel) {
    const fragment = document.createDocumentFragment();
    const header = node("span", "dr-header", patch.header.join("\n"));
    header.hidden = true;
    fragment.append(header);
    const headings = node("span", "dr-headings");
    headings.append(heading("Repository", "dr-repository", "M2.5 2.25h11v11h-11z M2.5 5.25h11 M5 8h6 M5 10.75h4"), heading(sideLabel === "Home" ? "This computer" : "System", `dr-${sideLabel.toLowerCase()}`, sideLabel === "Home"
        ? "M1.75 7.5 8 2l6.25 5.5 M3.5 6.25v7.25h9V6.25 M6.5 13.5v-4h3v4"
        : "M2.25 3h11.5v8.5H2.25z M5 13.5h6 M8 11.5v2"));
    fragment.append(headings);
    for (const hunk of patch.hunks) {
        fragment.append(node("span", "dr-hunk", hunk.header));
        for (let i = 0; i < hunk.lines.length;) {
            if (hunk.lines[i].type === " ") {
                const line = hunk.lines[i++];
                fragment.append(row(line, line));
                continue;
            }
            const removed = [];
            const added = [];
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
function button(label, action) {
    const result = node("button", "", label);
    result.type = "button";
    result.addEventListener("click", action);
    return result;
}
function changeButton(label, step) {
    const result = button("", () => navigate(step));
    result.className = "icon-button";
    result.setAttribute("aria-label", label);
    const glyph = icon(step === -1 ? "m3 10 5-5 5 5" : "m3 6 5 5 5-5");
    glyph.setAttribute("class", "workspace-glyph");
    result.append(glyph);
    return result;
}
function updateNavigation() {
    previous.disabled = current <= 0;
    next.disabled = current >= changes.length - 1;
    position.textContent = `Change ${current + 1} of ${changes.length}`;
    anchorTop = changes[current].getBoundingClientRect().top - pre.getBoundingClientRect().top;
}
function navigate(step) {
    current = Math.max(0, Math.min(changes.length - 1, current + step));
    pre.scrollTop += changes[current].getBoundingClientRect().top - pre.getBoundingClientRect().top;
    updateNavigation();
}
pre.addEventListener("scroll", () => {
    if (!changes.length)
        return;
    const top = pre.getBoundingClientRect().top;
    let index = 0;
    if (pre.scrollHeight > pre.clientHeight && pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 2) {
        index = changes.length - 1;
    }
    else {
        for (let i = 1; i < changes.length; i++) {
            if (changes[i].getBoundingClientRect().top > top + 2)
                break;
            index = i;
        }
    }
    if (index !== current) {
        current = index;
        updateNavigation();
    }
    else {
        anchorTop = changes[current].getBoundingClientRect().top - top;
    }
});
window.addEventListener("resize", () => {
    if (!changes.length)
        return;
    pre.scrollTop += changes[current].getBoundingClientRect().top - pre.getBoundingClientRect().top - anchorTop;
});
export function clear() {
    changes = [];
    current = -1;
    tools.hidden = true;
    tools.replaceChildren();
    pre.className = "";
    pre.textContent = "";
}
export function show(text, sideLabel = "Home") {
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
    tools.append(toggle, metadata, position, previous, next);
    tools.hidden = false;
}
