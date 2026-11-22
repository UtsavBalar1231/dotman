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
  const position = button("position");
  const save = button("save"), closeButton = button("cancel"), discard = button("discard");
  const wrap = button("wrap"), undo = button("undo"), redo = button("redo");
  const session = new AbortController();
  const { signal } = session;
  let buffer: Awaited<ReturnType<typeof createBuffer>>;
  let writing = false;
  let crlf = false;

  function controls() {
    const locked = !buffer || writing;
    tools.disabled = locked;
    indent.disabled = position.disabled = locked;
    closeButton.disabled = writing;
    save.disabled = locked || !buffer?.dirty;
    undo.disabled = !buffer?.canUndo;
    redo.disabled = !buffer?.canRedo;
    position.textContent = buffer?.position ?? "";
  }
  function close() {
    session.abort();
    buffer?.destroy();
    buffer = undefined;
    dialog.close();
  }
  function requestClose() {
    if (writing) return;
    if (!buffer?.dirty) { close(); return; }
    discard.hidden = false;
    message.textContent = "Discard your unsaved edits?";
    discard.focus();
  }
  async function write() {
    if (!buffer?.dirty || writing) return;
    writing = true;
    controls();
    host.inert = true;
    discard.hidden = true;
    message.textContent = "Waiting for save confirmation…";
    const contents = buffer.text();
    try {
      await invoke("save_source", { id: entry.id, text: crlf ? contents.replaceAll("\n", "\r\n") : contents });
    } catch (error) {
      writing = false;
      host.inert = false;
      message.textContent = String(error);
      controls();
      buffer.focus();
      return;
    }
    close();
    await saved();
  }
  closeButton.addEventListener("click", requestClose, { signal });
  discard.addEventListener("click", close, { signal });
  save.addEventListener("click", write, { signal });
  dialog.addEventListener("cancel", event => { event.preventDefault(); requestClose(); }, { signal });
  dialog.addEventListener("keydown", event => {
    if (event.key !== "Escape" || buffer?.hasPanel()) return;
    event.preventDefault();
    event.stopPropagation();
    requestClose();
  }, { capture: true, signal });
  dialog.addEventListener("keydown", event => {
    event.stopPropagation();
    if (event.defaultPrevented) return;
    if (event.key === "Escape") {
      event.preventDefault();
      requestClose();
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
  discard.hidden = true;
  host.inert = false;
  message.textContent = "Opening repository source…";
  controls();
  dialog.showModal();

  try {
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
        message.textContent = buffer.dirty ? "Unsaved changes" : "Saved in repository";
        discard.hidden = true;
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
