import { pathLabel } from "./file-tree.js";
import { icon, type IconName } from "./fluent-icons.js";
import type { TauriCore, TrackingAction, TrackingPreview } from "./protocol.js";

function reviewDescription(preview: TrackingPreview, stop: boolean, actionable: boolean) {
  if (stop) {
    return preview.folders.length
      ? "Your Home copies stay in place. This removes the listed sources and folder capture rules."
      : "Your Home copy stays in place. This removes the listed sources from the repository.";
  }
  if (!actionable) {
    if (!Object.keys(preview.held).length && preview.existing.length) return "These files are already tracked.";
    return "There are no new files to track in this selection.";
  }
  return preview.folders.length
    ? "Future syncs will also capture eligible new files in the selected folder on this machine."
    : "Add the listed files to this repository. You can sync them later.";
}

export function trackingFlow(invoke: TauriCore["invoke"], available: () => boolean, setBusy: (working: boolean) => Promise<void>,
  refresh: (message: string, focusKey?: string) => Promise<void>, selectedFolder: () => string | null) {
  function element<T extends HTMLElement>(id: string) {
    const value = document.getElementById(id);
    if (!value) throw new Error("Missing tracking control");
    return value as T;
  }
  const menu = element<HTMLDetailsElement>("track-menu");
  const drawer = element<HTMLDialogElement>("tracking-drawer");
  const apply = element<HTMLButtonElement>("tracking-apply");
  const close = element<HTMLButtonElement>("tracking-close");
  const status = element("tracking-message");
  const groups = element("tracking-groups");
  let pending: { id: string; action: TrackingAction; preview: TrackingPreview } | null = null;
  let working = false;
  let launcher: HTMLElement | null = null;

  function restoreFocus() {
    const target = launcher?.isConnected && launcher.getClientRects().length
      ? launcher : menu.querySelector("summary");
    target?.focus();
  }

  function group(label: string, glyph: IconName, paths: readonly string[], expanded: boolean,
    reasons?: Readonly<Record<string, string>>) {
    if (!paths.length) return;
    const details = document.createElement("details");
    details.open = expanded;
    if (reasons) details.className = "tracking-holds";
    const summary = document.createElement("summary");
    const count = document.createElement("span");
    count.className = "tracking-count";
    count.textContent = String(paths.length);
    summary.setAttribute("aria-label", `${label} (${paths.length})`);
    summary.append(icon(glyph), label, count, icon("chevron-right", "disclosure-glyph"));
    const list = document.createElement("ul");
    for (const path of paths) {
      const item = document.createElement("li");
      item.append(pathLabel(path));
      if (reasons) {
        const reason = document.createElement("span");
        reason.className = "tracking-reason";
        reason.textContent = reasons[path];
        item.append(reason);
      }
      list.append(item);
    }
    details.append(summary, list);
    groups.append(details);
  }
  function leave() {
    if (working) return;
    if (pending) void invoke("discard_review", {id: pending.id}).catch(() => {});
    pending = null;
    drawer.close();
    restoreFocus();
  }
  async function start(action: TrackingAction, key?: string) {
    if (!available() || working) return;
    const active = document.activeElement;
    launcher = active instanceof HTMLElement && active !== document.body && !menu.contains(active) ? active : menu.querySelector("summary");
    menu.open = false;
    working = true;
    await setBusy(true);
    let error = "";
    try {
      const result = await invoke("preview_tracking", {action, key: key ?? null});
      if (!result) return;
      const [id, preview] = result;
      pending = {id, action, preview};
      const stop = action === "stop_tracking";
      element("tracking-title").textContent = stop ? "Stop tracking" : "Review tracking";
      drawer.dataset.action = action;
      const actionable = preview.files.length + preview.folders.length + (stop ? preview.existing.length : 0) > 0;
      element("tracking-scope").textContent = reviewDescription(preview, stop, actionable);
      groups.replaceChildren();
      group(stop ? "Folder rules to remove" : "Track folder", "folder", preview.folders, true);
      const held = Object.keys(preview.held);
      group("Held back", "warning", held, true, preview.held);
      group(stop ? "Repository sources to remove" : "Files to track", "repository", preview.files, true);
      group(stop ? "Home paths kept" : "Already tracked", "home", preview.existing, !actionable && !held.length);
      apply.hidden = !actionable;
      apply.disabled = false;
      apply.textContent = stop ? "Stop tracking…" : "Track selection…";
      status.textContent = "";
      drawer.showModal();
      close.focus();
    } catch (cause) {
      error = typeof cause === "string" ? cause : "Tracking preview could not be opened.";
    } finally {
      working = false;
      await setBusy(false);
      if (error) await refresh(error);
      if (!drawer.open) restoreFocus();
    }
  }
  close.addEventListener("click", leave);
  drawer.addEventListener("cancel", event => { event.preventDefault(); leave(); });
  apply.addEventListener("click", async () => {
    if (!pending || working || apply.hidden) return;
    const {id, action, preview} = pending;
    pending = null;
    working = true;
    await setBusy(true);
    apply.disabled = true;
    close.disabled = true;
    let message: string;
    let focusKey: string | undefined;
    try {
      const theme = document.documentElement.dataset.theme;
      const dark = theme === "dark" || (theme !== "light" && window.matchMedia("(prefers-color-scheme: dark)").matches);
      const done = await invoke("apply_tracking", {id,dark});
      if (!done) message = "Tracking change cancelled.";
      else if (action === "stop_tracking") message = "Stopped tracking. Local copies kept.";
      else {
        focusKey = preview.files[0];
        const count = preview.files.length;
        const held = Object.keys(preview.held).length;
        message = `Tracked ${count} ${count === 1 ? "file" : "files"}${preview.folders.length ? " and enabled folder capture" : ""}.${held ? ` ${held} held back.` : ""}`;
      }
    } catch (cause) {
      message = typeof cause === "string" ? cause : "Tracking result unavailable. Check the refreshed file tree.";
    }
    working = false;
    apply.disabled = false;
    close.disabled = false;
    await setBusy(false);
    drawer.close();
    await refresh(message,focusKey);
    if (!focusKey) restoreFocus();
  });
  for (const [id,action] of [["track-files","track_files"],["track-folder","track_folder"],["untrack-folder","stop_tracking"]] as const) {
    element(id).addEventListener("click", () => void start(action, action === "stop_tracking" ? selectedFolder() ?? undefined : undefined));
  }
  menu.addEventListener("focusout", event => {
    if (!(event.relatedTarget instanceof Node) || !menu.contains(event.relatedTarget)) menu.open = false;
  });
  menu.addEventListener("keydown", event => {
    const trigger = menu.querySelector("summary");
    if (event.key === "Escape" && menu.open) {
      event.preventDefault();
      menu.open = false;
      trigger?.focus();
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    menu.open = true;
    const choices = Array.from(menu.querySelectorAll("button"));
    const current = choices.findIndex(button => button === document.activeElement);
    const next = event.key === "ArrowDown" ? current + 1 : current <= 0 ? choices.length - 1 : current - 1;
    choices[next % choices.length]?.focus();
  });
  return start;
}
