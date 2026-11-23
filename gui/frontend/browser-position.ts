import type { EntryView } from "./protocol.js";

export function fileFocusKey(entry: EntryView) {
  return JSON.stringify([entry.system, entry.key]);
}

function uniqueHeldRows(rows: HTMLElement) {
  const byPath = new Map<string, HTMLDetailsElement | null>();
  for (const row of rows.querySelectorAll<HTMLDetailsElement>("details[data-held-key]")) {
    const key = row.dataset.heldKey!;
    byPath.set(key, byPath.has(key) ? null : row);
  }
  return byPath;
}

export function browserPosition(rows: HTMLElement, overview: HTMLElement) {
  const expanded = Array.from(uniqueHeldRows(rows)).flatMap(([key, row]) => row?.open ? [key] : []);
  const focused = document.activeElement;
  const owner = rows.contains(focused) ? rows : overview.contains(focused) ? overview : null;
  const target = owner && focused?.closest<HTMLElement>("[data-file-focus], [data-folder-path]");
  const file = target?.dataset.fileFocus;
  const folder = target?.dataset.folderPath;
  const more = focused?.id === "more-comparisons";
  const scroll = { rows: rows.scrollTop, overview: overview.scrollTop };
  return {
    file,
    inOverview: owner === overview,
    restore(search: HTMLElement) {
      if (expanded.length) {
        const current = uniqueHeldRows(rows);
        for (const key of expanded) {
          const row = current.get(key);
          if (row) row.open = true;
        }
      }
      rows.scrollTop = scroll.rows;
      overview.scrollTop = scroll.overview;
      if (!owner || (!file && !folder && !more)) return;
      const selector = file ? `[data-file-focus="${CSS.escape(file)}"]`
        : folder ? `[data-folder-path="${CSS.escape(folder)}"]` : "#more-comparisons";
      const matches = owner.querySelectorAll<HTMLElement>(selector);
      const match = matches.length === 1 && matches[0].getClientRects().length ? matches[0] : search;
      match.focus({ preventScroll: true });
      if (match !== search) match.scrollIntoView({ block: "nearest", inline: "nearest" });
    },
  };
}
