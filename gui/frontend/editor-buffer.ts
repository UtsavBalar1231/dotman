import { searchPanel } from "./editor-search.js";
import { basicSetup } from "codemirror";
import { completeAnyWord, completionStatus } from "@codemirror/autocomplete";
import { indentWithTab, redo, redoDepth, undo, undoDepth } from "@codemirror/commands";
import { json } from "@codemirror/lang-json";
import { yaml } from "@codemirror/lang-yaml";
import { HighlightStyle, indentUnit, StreamLanguage, syntaxHighlighting } from "@codemirror/language";
import { lua } from "@codemirror/legacy-modes/mode/lua";
import { properties } from "@codemirror/legacy-modes/mode/properties";
import { python } from "@codemirror/legacy-modes/mode/python";
import { shell } from "@codemirror/legacy-modes/mode/shell";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { gotoLine, openSearchPanel, search, searchPanelOpen } from "@codemirror/search";
import { Compartment, countColumn, EditorState } from "@codemirror/state";
import { EditorView, getDialog, keymap } from "@codemirror/view";
import { tags } from "@lezer/highlight";

async function languageFor(path: string, firstLine: string) {
  const name = path.split("/").at(-1)?.toLowerCase() ?? "";
  if (/\.(?:sh|bash|zsh)$|^\.(?:bashrc|bash_profile|profile|zshrc|zlogin|zprofile|zshenv|env(?:\..*)?)$/.test(name) ||
      /^#!.*\b(?:ba|z|da|k)?sh\b/.test(firstLine)) return { name: "Shell", extension: StreamLanguage.define(shell) };
  if (name.endsWith(".toml")) return { name: "TOML", extension: StreamLanguage.define(toml) };
  if (/\.ya?ml$/.test(name)) return { name: "YAML", extension: yaml() };
  if (/\.jsonc?$/.test(name)) return { name: "JSON", extension: json() };
  if (/\.[cm]?[jt]sx?$/.test(name)) {
    const { javascript } = await import("@codemirror/lang-javascript");
    const typescript = /\.[cm]?tsx?$/.test(name);
    return { name: typescript ? "TypeScript" : "JavaScript", extension: javascript({ typescript, jsx: name.endsWith("x") }) };
  }
  if (name.endsWith(".lua")) return { name: "Lua", extension: StreamLanguage.define(lua) };
  if (name.endsWith(".py") || /^#!.*\bpython[\d.]*\b/.test(firstLine)) return { name: "Python", extension: StreamLanguage.define(python) };
  if (/\.(?:ini|conf|service|timer|socket|desktop|properties)$|^\.(?:gitconfig|editorconfig)$/.test(name) ||
      path.endsWith("/git/config")) return { name: "INI", extension: StreamLanguage.define(properties) };
  return { name: "Plain text", extension: [] };
}

const syntax = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.meta], color: "var(--accent)" },
  { tag: [tags.string, tags.special(tags.string), tags.regexp], color: "var(--added)" },
  { tag: [tags.number, tags.bool, tags.null, tags.atom], color: "var(--amber)" },
  { tag: [tags.propertyName, tags.definition(tags.variableName), tags.typeName], color: "light-dark(#135e76,#85d0df)" },
  { tag: [tags.comment, tags.processingInstruction], color: "var(--muted)", fontStyle: "italic" },
  { tag: [tags.invalid], textDecoration: "underline wavy var(--removed)" },
]);

const theme = EditorView.theme({
  "&": { height: "100%", background: "var(--surface)", color: "var(--ink)", fontSize: "0.95rem" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": { fontFamily: "var(--font-workbench)", lineHeight: "1.75", overflow: "auto" },
  ".cm-content": { padding: "1rem 0", caretColor: "var(--accent)" },
  ".cm-line": { padding: "0 .85rem" },
  ".cm-gutters": { background: "var(--surface)", color: "var(--muted)", border: "none", paddingLeft: ".45rem" },
  ".cm-activeLine,.cm-activeLineGutter": { background: "var(--paper)" },
  ".cm-activeLineGutter": { color: "var(--accent)" },
  ".cm-cursor,.cm-dropCursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground,.cm-selectionBackground": { background: "var(--text-selection)" },
  ".cm-searchMatch,.cm-selectionMatch": { background: "var(--selected)", outline: "1px solid var(--rule)" },
  ".cm-searchMatch.cm-searchMatch-selected,.cm-matchingBracket": { outline: "1px solid var(--accent)" },
  ".cm-panels,.cm-tooltip": { color: "var(--ink)", background: "var(--paper)", borderColor: "var(--partition)" },
  ".source-search": { padding: ".6rem clamp(.65rem, 2vw, 1.35rem)", fontFamily: "var(--font-workbench)" },
  ".source-search > div:not([hidden])": { display: "flex", alignItems: "center", flexWrap: "wrap", gap: ".25rem" },
  ".source-search > div + div": { marginTop: ".3rem" },
  ".source-search input": { flex: "1 1 7rem", minWidth: "5rem", color: "var(--ink)", background: "var(--surface)", border: "1px solid var(--rule)", borderRadius: "5px", padding: ".42rem .65rem" },
  ".source-search button": { minHeight: "2rem", cursor: "pointer", color: "var(--muted)", background: "transparent", border: "1px solid transparent", borderRadius: "5px", padding: ".23rem .5rem" },
  ".source-search button:enabled:hover,.source-search button[aria-expanded=true],.source-search button[aria-pressed=true]": { color: "var(--accent)", background: "var(--selected)" },
  ".source-search button:disabled": { opacity: ".45", cursor: "default" },
  ".source-search svg": { display: "block", width: "1.1rem", height: "1.1rem", fill: "currentColor" },
  ".source-search output": { color: "var(--muted)", marginLeft: "auto", fontSize: ".78rem" },
  ".source-search-options button": { fontSize: ".76rem" },
  ".source-search :focus-visible": { outline: "2px solid var(--accent)", outlineOffset: "1px" },
  ".cm-panel.cm-goto-line": { padding: ".65rem 1.8rem .65rem .65rem", font: "inherit" },
  ".cm-panel input,.cm-panel button": { fontFamily: "inherit", fontSize: ".85rem", color: "inherit", accentColor: "var(--accent)" },
  ".cm-textfield,.cm-button": { background: "var(--surface)", border: "1px solid var(--rule)", borderRadius: "5px", padding: ".32rem .45rem", maxWidth: "75%" },
  ".cm-panel label": { color: "var(--muted)" },
  ".cm-tooltip-autocomplete ul li[aria-selected]": { background: "var(--selected)", color: "var(--ink)" },
});

export async function createBuffer(host: HTMLElement, contents: string, path: string, signal: AbortSignal, changed: (docChanged: boolean) => void) {
  const language = await languageFor(path, contents.split("\n", 1)[0]);
  if (signal.aborted) return;
  const root = host.shadowRoot ?? host.attachShadow({ mode: "open" });
  const wrap = new Compartment();
  const indentation = new Compartment();
  const leading = /^([ \t]+)\S/m.exec(contents)?.[1];
  const unit = leading?.startsWith("\t") ? "\t" : " ".repeat(leading && leading.length >= 4 ? 4 : 2);
  const state = EditorState.create({
    doc: contents,
    extensions: [
      basicSetup, language.extension, theme, syntaxHighlighting(syntax), search({ top: true, createPanel: searchPanel }),
      EditorState.tabSize.of(4), indentation.of(indentUnit.of(unit)), wrap.of([]),
      EditorState.languageData.of(() => [{ autocomplete: completeAnyWord }]),
      EditorView.contentAttributes.of({ "aria-label": "Repository source text", spellcheck: "false", autocapitalize: "off" }),
      EditorView.domEventHandlers({
        beforeinput(event, view) {
          const { from, to } = view.state.selection.main;
          const text = event.data;
          if (from === to || event.inputType !== "insertText" || !text || !event.cancelable || event.isComposing) return false;
          // Replacing highlighted text through the DOM can copy its color into a blocked style attribute.
          const insert = () => view.state.update(view.state.replaceSelection(text), { userEvent: "input.type", scrollIntoView: true });
          if (!view.state.facet(EditorView.inputHandler).some(handler => handler(view, from, to, text, insert))) view.dispatch(insert());
          return true;
        },
      }),
      keymap.of([indentWithTab]),
      EditorView.updateListener.of(update => { if (update.docChanged || update.selectionSet) changed(update.docChanged); }),
    ],
  });
  const view = new EditorView({ state, root, parent: root });
  return {
    language: language.name, unit,
    get dirty() { return !view.state.doc.eq(state.doc); },
    get position() {
      const head = view.state.selection.main.head;
      const line = view.state.doc.lineAt(head);
      return `Ln ${line.number}, Col ${countColumn(line.text.slice(0, head - line.from), view.state.tabSize) + 1}`;
    },
    get canUndo() { return undoDepth(view.state) > 0; },
    get canRedo() { return redoDepth(view.state) > 0; },
    text: () => view.state.doc.toString(),
    focus: () => view.focus(),
    hasPanel: () => searchPanelOpen(view.state) || !!getDialog(view, "cm-goto-line") || !!completionStatus(view.state),
    undo: () => { undo(view); view.focus(); },
    redo: () => { redo(view); view.focus(); },
    find: () => openSearchPanel(view),
    jump: () => gotoLine(view),
    wrap: (on: boolean) => view.dispatch({ effects: wrap.reconfigure(on ? EditorView.lineWrapping : []) }),
    indent: (value: string) => view.dispatch({ effects: indentation.reconfigure(indentUnit.of(value)) }),
    destroy: () => view.destroy(),
  };
}
