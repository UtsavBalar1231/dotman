import { useEffect, useState, useSyncExternalStore } from "react";
import { getWorkspaceView, subscribeToWorkspace } from "../workspace-view.js";
import { Button } from "./button.js";
import { Icon } from "./symbols.js";

function BrandNavigation({ current, busy }: { current: boolean; busy: boolean }) {
  const [hint, setHint] = useState(false);
  useEffect(() => {
    if (!hint) return;
    const dismiss = (event: KeyboardEvent) => {
      if (event.key === "Escape") setHint(false);
    };
    document.addEventListener("keydown", dismiss);
    return () => document.removeEventListener("keydown", dismiss);
  }, [hint]);

  return (
    <h1 className="brand-anchor"
      onPointerEnter={event => setHint(event.pointerType !== "touch")}
      onPointerLeave={() => setHint(false)}>
      <Button id="nav-files" className="brand" aria-label="Files"
        aria-current={current ? "page" : undefined}
        aria-describedby={hint && !busy ? "brand-hint" : undefined} disabled={busy}
        onFocus={event => setHint(event.currentTarget.matches(":focus-visible"))}
        onBlur={() => setHint(false)} onClick={() => setHint(false)}>
        <svg className="workspace-mark" aria-hidden="true" focusable="false"><use href="#pair-mark" /></svg>
        Dotman
      </Button>
      {hint && !busy && <span id="brand-hint" role="tooltip">
        <span><Icon name="folder" />{current ? "File browser" : "Back to files"}</span>
      </span>}
    </h1>
  );
}

export function WorkspaceHeader() {
  const { page, repository, status, busy, cancel, queued, cancelling } =
    useSyncExternalStore(subscribeToWorkspace, getWorkspaceView);
  const title = { files: "Files", settings: "Settings", review: "Review changes" }[page];
  return (
    <header className="topbar">
      <BrandNavigation current={page === "files"} busy={busy} />
      <span className="toolbar-repo" title={repository}>{repository.split("/").filter(Boolean).at(-1)}</span>
      <div className="page-status">
        <h2 id="page-heading" className="page-heading" tabIndex={-1}>{title}</h2>
        <p id="message" role="status" aria-live="polite" tabIndex={-1}>{status}</p>
        <Button id="review-decisions" hidden={queued === null} disabled={busy}>{queued}</Button>
      </div>
      <div className="actions">
        <Button id="refresh" className="icon-button" aria-label="Refresh status" title="Refresh status" disabled={busy}>
          <Icon name="refresh" />
        </Button>
        <Button id="nav-settings" className="icon-button" aria-label="Settings" title="Workspace settings"
          aria-current={page === "settings" ? "page" : undefined} disabled={busy}><Icon name="settings" /></Button>
        <Button id="nav-review" disabled={busy} className="with-glyph" aria-label="Review changes" title="Review outgoing changes">
          <Icon name="review" /><span>Review changes</span>
        </Button>
        <Button id="cancel" hidden={cancel === null} disabled={cancel === null || cancelling}>{cancel}</Button>
      </div>
    </header>
  );
}
