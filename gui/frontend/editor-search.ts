import {
  closeSearchPanel, findNext, findPrevious, getSearchQuery, replaceAll, replaceNext,
  SearchQuery, setSearchQuery,
} from "@codemirror/search";
import { runScopeHandlers } from "@codemirror/view";
import type { EditorView, Panel } from "@codemirror/view";
import { fluentPaths } from "./fluent-icons.js";

function field(label: string) {
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = label;
  input.setAttribute("aria-label", label);
  input.spellcheck = false;
  return input;
}

function action(label: string, run: () => void, glyph?: "chevron-up" | "chevron-down" | "close") {
  const button = document.createElement("button");
  button.type = "button";
  button.title = label;
  button.setAttribute("aria-label", label);
  button.onclick = run;
  if (glyph) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    const path = document.createElementNS(svg.namespaceURI, "path");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("aria-hidden", "true");
    path.setAttribute("d", fluentPaths[glyph]);
    svg.append(path);
    button.append(svg);
  } else button.textContent = label;
  return button;
}

export function searchPanel(view: EditorView): Panel {
  const dom = document.createElement("div");
  dom.className = "source-search";
  dom.setAttribute("role", "search");
  const find = field("Find");
  find.setAttribute("main-field", "true");
  const replace = field("Replace");
  const result = document.createElement("output");
  const flags = {
    caseSensitive: action("Match case", () => toggle("caseSensitive")),
    wholeWord: action("Whole word", () => toggle("wholeWord")),
    regexp: action("Regex", () => toggle("regexp")),
  };
  function toggle(key: keyof typeof flags) {
    flags[key].setAttribute("aria-pressed", String(flags[key].getAttribute("aria-pressed") !== "true"));
    commit();
  }
  function commit() {
    const pressed = (key: keyof typeof flags) => flags[key].getAttribute("aria-pressed") === "true";
    const query = new SearchQuery({
      search: find.value, replace: replace.value,
      caseSensitive: pressed("caseSensitive"), wholeWord: pressed("wholeWord"), regexp: pressed("regexp"),
    });
    view.dispatch({ effects: setSearchQuery.of(query) });
  }
  const queryRow = document.createElement("div");
  const prev = action("Previous match", () => findPrevious(view), "chevron-up");
  const next = action("Next match", () => findNext(view), "chevron-down");
  queryRow.append(find, prev, next, action("Close search", () => closeSearchPanel(view), "close"));
  const replacements = document.createElement("div");
  replacements.hidden = true;
  const one = action("Replace next", () => { replaceNext(view); replace.focus(); });
  const all = action("Replace all", () => { replaceAll(view); replace.focus(); });
  replacements.append(replace, one, all);
  const options = document.createElement("div");
  options.className = "source-search-options";
  const disclosure = action("Replace", () => {
    replacements.hidden = !replacements.hidden;
    disclosure.setAttribute("aria-expanded", String(!replacements.hidden));
    if (!replacements.hidden) replace.focus();
  });
  disclosure.setAttribute("aria-expanded", "false");
  options.append(disclosure, ...Object.values(flags), result);
  dom.append(queryRow, options, replacements);
  find.oninput = replace.oninput = commit;
  dom.onkeydown = event => {
    if (runScopeHandlers(view, event, "search-panel")) event.preventDefault();
    else if (event.key === "Enter" && (event.target === find || event.target === replace)) {
      event.preventDefault();
      if (event.target === replace) replaceNext(view);
      else if (event.shiftKey) findPrevious(view);
      else findNext(view);
    }
  };
  function sync() {
    const query = getSearchQuery(view.state);
    find.value = query.search;
    replace.value = query.replace;
    for (const key of Object.keys(flags) as (keyof typeof flags)[]) {
      flags[key].setAttribute("aria-pressed", String(query[key]));
    }
    let matches = 0;
    if (query.valid) {
      const cursor = query.getCursor(view.state);
      while (matches <= 100 && !cursor.next().done) matches++;
    }
    result.textContent = !query.search ? "" : !query.valid ? "Invalid pattern"
      : matches > 100 ? "100+ matches" : matches === 1 ? "1 match" : `${matches} matches`;
    prev.disabled = next.disabled = one.disabled = all.disabled = !matches;
  }
  return {
    dom, top: true,
    mount() { sync(); find.focus(); find.select(); },
    update(update) {
      if (update.docChanged || getSearchQuery(update.startState) !== getSearchQuery(update.state)) sync();
    },
  };
}
