import type { createBuffer } from "./editor-buffer.js";
import type { EntryView, TauriCore } from "./protocol.js";

export async function editSource(
  entry: EntryView, invoke: TauriCore["invoke"], saved: () => Promise<void>,
) {
  const dialog = document.querySelector<HTMLDialogElement>("#source-editor")!;
  if (dialog.open) return;
  const button = (name: string) => dialog.querySelector<HTMLButtonElement>(".source-" + name)!;
  const host = dialog.querySelector<HTMLElement>("#source-buffer")!;
  const tools = dialog.querySelector("fieldset")!;
  const message = dialog.querySelector<HTMLElement>("[role=status]")!;
  const indent = dialog.querySelector<HTMLSelectElement>(".source-indent")!;
  const exitChoices = dialog.querySelector<HTMLElement>(".source-exit")!;
  const copy = button("copy");
  const resume = button("continue");
  const saveExit = button("save-exit");
  const position = button("position");
  const save = button("save"), closeButton = button("cancel"), discard = button("discard");
  const wrap = button("wrap"), undo = button("undo"), redo = button("redo");
  const session = new AbortController();
  const { signal } = session;
  let buffer: Awaited<ReturnType<typeof createBuffer>>;
  let writing = false;
  let crlf = false;
  let quitRequested = false;
  let unlisten: (() => void) | undefined;
  const nativeWindow = window.__TAURI__.window.getCurrentWindow();

  function controls() {
    const locked = !buffer || writing;
    tools.disabled = locked;
    indent.disabled = position.disabled = copy.disabled = locked;
    closeButton.disabled = writing;
    save.disabled = locked || !buffer?.dirty;
    undo.disabled = !buffer?.canUndo;
    redo.disabled = !buffer?.canRedo;
    position.textContent = buffer?.position ?? "";
  }
  function close() {
    unlisten?.();
    session.abort();
    buffer?.destroy();
    buffer = undefined;
    dialog.close();
  }
  function requestClose(quit = false) {
    if (writing) return;
    if (!buffer?.dirty) { close(); return; }
    quitRequested = quit;
    copy.hidden = true;
    exitChoices.hidden = false;
    discard.textContent = quit ? "Discard and quit" : "Discard edits";
    saveExit.textContent = quit ? "Save and quit…" : "Save and close…";
    message.textContent = quit ? "Save your edits before quitting Dotman?" : "Save your edits before closing this file?";
    resume.focus();
  }
  function keepEditing() {
    quitRequested = false;
    exitChoices.hidden = true;
    message.textContent = "Unsaved changes";
    buffer?.focus();
  }
  function escapeEditor() {
    if (exitChoices.hidden) requestClose();
    else keepEditing();
  }
  async function quitWindow() {
    writing = true;
    host.inert = true;
    controls();
    exitChoices.hidden = true;
    message.textContent = "Closing Dotman…";
    try {
      await nativeWindow.destroy();
      return true;
    } catch {
      writing = false;
      host.inert = false;
      quitRequested = false;
      controls();
      message.textContent = "Could not close the window.";
      buffer?.focus();
      return false;
    }
  }
  function withLineEndings(text: string) {
    return crlf ? text.replaceAll("\n", "\r\n") : text;
  }
  async function copyDraft() {
    if (!buffer || writing) return;
    try {
      await navigator.clipboard.writeText(withLineEndings(buffer.text()));
      if (!signal.aborted) message.textContent = "Draft copied to the clipboard.";
    } catch {
      if (signal.aborted) return;
      buffer.focus();
      message.textContent = "Clipboard unavailable. Press Ctrl+A, then Ctrl+C to copy the draft.";
    }
  }
  async function write(quit = false) {
    if (!buffer?.dirty || writing) return;
    writing = true;
    controls();
    host.inert = true;
    exitChoices.hidden = true;
    message.textContent = "Waiting for save confirmation…";
    copy.hidden = true;
    try {
      await invoke("save_source", { id: entry.id, text: withLineEndings(buffer.text()) });
    } catch (error) {
      writing = false;
      quitRequested = false;
      host.inert = false;
      message.textContent = String(error);
      copy.hidden = false;
      controls();
      buffer.focus();
      return;
    }
    if (quit && await quitWindow()) return;
    close();
    await saved();
  }
  closeButton.addEventListener("click", () => requestClose(), { signal });
  copy.addEventListener("click", copyDraft, { signal });
  resume.addEventListener("click", keepEditing, { signal });
  discard.addEventListener("click", () => { if (quitRequested) void quitWindow(); else close(); }, { signal });
  saveExit.addEventListener("click", () => void write(quitRequested), { signal });
  save.addEventListener("click", () => void write(), { signal });
  dialog.addEventListener("cancel", event => { event.preventDefault(); escapeEditor(); }, { signal });
  dialog.addEventListener("keydown", event => {
    if (event.key !== "Escape" || buffer?.hasPanel()) return;
    event.preventDefault();
    event.stopPropagation();
    escapeEditor();
  }, { capture: true, signal });
  dialog.addEventListener("keydown", event => {
    event.stopPropagation();
    if (event.defaultPrevented) return;
    if (event.key === "Escape") {
      event.preventDefault();
      escapeEditor();
    } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
      event.preventDefault();
      void write();
    }
  }, { signal });

  const path = entry.machine_file?.selected || entry.key;
  const split = path.lastIndexOf("/");
  dialog.querySelector("h2")!.textContent = path.slice(split + 1);
  dialog.querySelector("#editor-directory")!.textContent = split < 0 ? "Repository" : path.slice(0, split);
  dialog.querySelector(".source-language")!.textContent = "";
  dialog.querySelector(".source-eol")!.textContent = "";
  wrap.setAttribute("aria-pressed", "false");
  exitChoices.hidden = true;
  copy.hidden = true;
  host.inert = false;
  message.textContent = "Opening repository source…";
  controls();
  dialog.showModal();

  try {
    unlisten = await nativeWindow.onCloseRequested(event => {
      if (writing || buffer?.dirty) {
        event.preventDefault();
        requestClose(true);
      }
    });
    if (signal.aborted) { unlisten(); return; }
    const [contents, editor] = await Promise.all([invoke("open_source", { id: entry.id }), import("./editor-buffer.js")]);
    if (signal.aborted) return;
    const withoutPairs = contents.replaceAll("\r\n", "");
    crlf = contents.includes("\r\n");
    if (withoutPairs.includes("\r") || (crlf && withoutPairs.includes("\n"))) {
      throw new Error("This file uses mixed or legacy line endings. Its source is unchanged.");
    }
    buffer = await editor.createBuffer(host, contents.replaceAll("\r\n", "\n"), entry.key, signal, docChanged => {
      if (!buffer) return;
      if (docChanged) {
        quitRequested = false;
        message.textContent = buffer.dirty ? "Unsaved changes" : "Saved in repository";
        exitChoices.hidden = true;
      }
      controls();
    });
    if (!buffer) return;
    indent.value = buffer.unit === "\t" ? "tab" : String(buffer.unit.length);
    dialog.querySelector(".source-language")!.textContent = buffer.language;
    dialog.querySelector(".source-eol")!.textContent = crlf ? "UTF-8 · CRLF" : "UTF-8 · LF";
    message.textContent = "Saved in repository";
    controls();
    buffer.focus();
  } catch (error) {
    if (!signal.aborted) message.textContent = String(error);
    return;
  }

  undo.addEventListener("click", buffer.undo, { signal });
  redo.addEventListener("click", buffer.redo, { signal });
  button("find").addEventListener("click", buffer.find, { signal });
  position.addEventListener("click", buffer.jump, { signal });
  wrap.addEventListener("click", () => {
    const on = wrap.getAttribute("aria-pressed") !== "true";
    buffer?.wrap(on);
    wrap.setAttribute("aria-pressed", String(on));
    buffer?.focus();
  }, { signal });
  indent.addEventListener("change", () => {
    buffer?.indent(indent.value === "tab" ? "\t" : indent.value === "4" ? "    " : "  ");
    buffer?.focus();
  }, { signal });
}
