import { useSyncExternalStore } from "react";
import { getWorkspaceView, subscribeToWorkspace } from "../workspace-view.js";
import { Button } from "./button.js";
import { Icon } from "./symbols.js";

const destinations = [
  ["files", "Files", "folder"],
  ["review", "Review", "review"],
  ["settings", "Settings", "settings"],
] as const;

export function WorkspaceHeader() {
  const { page, status, busy, cancel, queued, cancelling } = useSyncExternalStore(subscribeToWorkspace, getWorkspaceView);
  const title = destinations.find(([id]) => id === page)?.[1];
  return (
    <header className="topbar">
      <div className="brand">
        <svg className="workspace-mark" aria-hidden="true" focusable="false"><use href="#pair-mark"/></svg>
        <h1>Dotman</h1>
      </div>
      <nav className="page-nav" aria-label="Workspace">
        {destinations.map(([id, label, icon]) => (
          <Button key={id} id={`nav-${id}`} className="with-glyph" disabled={busy} aria-current={page === id ? "page" : undefined}>
            <Icon name={icon} />
            {label}
          </Button>
        ))}
      </nav>
      <div className="page-status">
        <h2 id="page-heading" className="page-heading" tabIndex={-1}>{title}</h2>
        <p id="message" role="status" aria-live="polite" tabIndex={-1}>{status}</p>
        <Button id="review-decisions" hidden={queued === null} disabled={busy}>{queued}</Button>
      </div>
      <div className="actions">
        <Button id="refresh" className="icon-button" aria-label="Refresh status" title="Refresh status" disabled={busy}>
          <Icon name="refresh" />
        </Button>
        <Button id="cancel" hidden={cancel === null} disabled={cancel === null || cancelling}>{cancel}</Button>
      </div>
    </header>
  );
}
