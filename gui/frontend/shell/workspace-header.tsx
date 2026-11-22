import { useSyncExternalStore } from "react";
import { getWorkspaceView, subscribeToWorkspace } from "../workspace-view.js";
import { Button } from "./button.js";
import { Icon } from "./symbols.js";

export function WorkspaceHeader() {
  const { page, repository, status, busy, cancel, queued, cancelling } =
    useSyncExternalStore(subscribeToWorkspace, getWorkspaceView);
  const title = { files: "Files", settings: "Settings", review: "Review changes" }[page];
  return (
    <header className="topbar">
      <h1>
        <Button id="nav-files" className="brand" aria-label="Files" title="Go to files" disabled={busy}>
          <svg className="workspace-mark" aria-hidden="true" focusable="false"><use href="#pair-mark" /></svg>
          Dotman
        </Button>
      </h1>
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
