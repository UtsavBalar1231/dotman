import { DestinationSettings } from "./destination-settings.js";
import { useSyncExternalStore } from "react";
import { getWorkspaceView, subscribeToWorkspace } from "../workspace-view.js";
import { ThemeSelect } from "./theme-select.js";
import { Icon } from "./symbols.js";
export function Settings() {
const {page, repository} = useSyncExternalStore(subscribeToWorkspace, getWorkspaceView);
return (
    <section id="settings" hidden aria-labelledby="page-heading">
      <div className="settings-section">
        <h3>Appearance</h3>
        <ThemeSelect />
      </div>
      <details id="context" hidden>
        <summary>
          <span className="repository-label with-glyph"><Icon name="repository" />Repository</span>
          <span id="repository-path"></span>
          <span className="paths-label with-glyph"><span>Resolved paths</span><Icon name="chevron-right" className="disclosure-glyph" /></span>
        </summary>
        <button id="open-repository" type="button">Show repository folder</button>
        <p id="repository-folder-feedback" role="status"></p>
        <dl id="paths"></dl>
      </details>
      <p id="paths-unavailable">Resolved paths are unavailable. Refresh status to try again.</p>
      {page === "settings" && repository && <DestinationSettings key={repository} />}
      <details id="policy-controls">
        <summary><Icon name="chevron-right" className="disclosure-glyph" />Local capture policy</summary>
        <p>Review directory or retained-ignore trust for future sync. Nothing is captured now.</p>
        <div className="actions">
          <button id="policy-directories" type="button">Review directories…</button>
          <button id="policy-ignores" type="button">Review retained ignore rules…</button>
        </div>
        <p id="policy-message" role="status" aria-live="polite"></p>
      </details>
      <details id="machine-files" hidden>
        <summary><Icon name="chevron-right" className="disclosure-glyph" />Machine files</summary>
        <div id="machine-file-details"></div>
        <button id="review-identity" type="button" hidden>Review identity change</button>
      </details>
      <section id="age-controls" hidden aria-labelledby="age-title">
      <details>
        <summary id="age-title"><Icon name="chevron-right" className="disclosure-glyph" />Age identity</summary>
        <p>Create a new identity or import one from a local file. Recipient changes require separate confirmation.</p>
        <div className="actions">
          <button id="create-age" type="button">Create identity…</button>
          <button id="import-age" type="button">Import identity…</button>
        </div>
      </details>
      <p id="age-message" role="status" aria-live="polite"></p>
      </section>
      <p id="age-unavailable">Age identity controls are unavailable until status is loaded.</p>
    </section>

);
}
