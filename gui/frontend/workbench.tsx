import { WorkspaceHeader } from "./shell/workspace-header.js";
import { Icon, Symbols } from "./shell/symbols.js";
import { Settings } from "./shell/settings.js";
import { StatusPanels } from "./shell/status-panels.js";

export function Workbench() {
return <>
<Symbols />
  <main>
    <WorkspaceHeader />
    <p id="notice" className="notice" hidden>DOTMAN_AGE_KEY was removed before launch. Encrypted files may be locked; use a key file or import an identity with dotman.</p>
<StatusPanels />
    <div className="desk">
    <section id="entries" hidden aria-labelledby="files-title">
      <h2 id="files-title">File browser</h2>
      <p className="queue-meta"><span id="remote"></span><span id="clean"></span></p>
      <button id="review-hooks" type="button" hidden>Review pending hooks</button>
      <label htmlFor="search">Search files</label>
      <div className="file-search">
        <Icon name="search" />
        <input id="search" type="search" placeholder="Name, path or status" autoComplete="off" />
      </div>
      <div className="nav-modes" role="group" aria-label="Group files">
        <button id="group-folders" className="with-glyph" aria-pressed="false"><Icon name="folder" />All files</button>
        <button id="group-status" className="with-glyph" aria-pressed="true"><Icon name="changed" />Changed</button>
      </div>
      <p id="held-counts" hidden></p>
      <div id="rows" tabIndex={0} aria-label="File change queue"></div>
     </section>
     <section className="desk-guide" aria-labelledby="guide-title">
        <h2 id="guide-title">File comparison</h2>
        <p id="guide-description"></p>
        <ul id="attention-list" className="attention-list"></ul>
     </section>
    <section id="review" hidden aria-labelledby="review-title">
      <div className="section-head"><h2 id="review-title">Review outgoing sync</h2><button id="close-review" className="icon-button" aria-label="Close review" title="Close review"><Icon name="close" /></button></div>
      <div id="review-body">
      <p>This read-only preview does not fetch or predict upstream changes. A changed hook table may require fresh approval. Sync without hooks writes only after native confirmation.</p>
      <p id="review-remote"></p>
      <p id="review-message" role="status" aria-live="polite" tabIndex={-1}></p>
      <div id="review-items"></div>
      <div id="line-summary" hidden><p id="selected-effect"></p><ul id="selected-lines"></ul><button id="leave-lines" type="button">Leave selected-only mode</button></div>
      <div id="commit-editor" hidden>
        <label htmlFor="commit-message">Commit message (optional)</label>
        <p id="commit-hint">Used for a new commit if one is made. Edits require native confirmation. Up to 1024 UTF-8 bytes and six lines; no control or invisible formatting characters.</p>
        <textarea id="commit-message" rows={4} aria-describedby="commit-hint commit-error"></textarea>
        <p id="commit-error" role="status" aria-live="polite"></p>
      </div>
      </div>
      <div className="review-action-strip">
      <button id="sync-reviewed" hidden>Sync without hooks</button>
      <button id="sync-mass-delete" type="button" hidden>Review and approve mass deletion</button>
      <button id="sync-lines" type="button" hidden>Commit selected lines</button>
      </div>
    </section>
    <section id="diff" hidden aria-labelledby="diff-key">
      <div className="section-head">
        <div className="comparison-title" role="group" aria-label="File and status" tabIndex={0}>
          <h2 id="diff-key"></h2>
          <p id="diff-context"></p>
        </div>
        <button id="close-diff" className="with-glyph" type="button">Back to files</button>
      </div>
      <div className="comparison-content">
        <p id="diff-message" role="status" aria-live="polite"></p>
        <div className="comparison-reader">
          <div id="reader-tools" hidden></div>
          <pre id="diff-text" tabIndex={0} aria-label="Diff text"></pre>
        </div>
        <fieldset id="line-edits" hidden><legend>Choose changes to commit</legend><p>Checking a change enters selected-only mode. Replacements include both sides.</p><div id="line-edit-list"></div></fieldset>
        <div id="source-edit-details"></div>
      </div>
      <section id="replacement-review" hidden aria-labelledby="replacement-title">
        <div className="replacement-content" role="group" tabIndex={0} aria-label="Replacement details">
          <h3 id="replacement-title" tabIndex={-1}>Replace the local copy?</h3>
          <dl className="replacement-paths">
            <div><dt>Copy from repository</dt><dd id="replacement-source"></dd></div>
            <div><dt>Replace on this computer</dt><dd id="replacement-target"></dd></div>
          </dl>
          <p>Applying this decision backs up the local file and replaces it with the repository copy. The repository stays unchanged.</p>
          <p>Add this file to your decisions, then confirm the complete list before anything is written.</p>
        </div>
        <div className="actions">
          <button id="replacement-back" type="button">Back to comparison</button>
          <button id="replacement-batch" type="button" className="review-action">Add to file decisions</button>
        </div>
      </section>
      <div className="diff-action-strip">
        <p id="diff-selection" role="status" aria-live="polite"></p>
        <div className="comparison-actions">
          <div id="diff-actions" className="actions"></div>
          <button id="next-undecided" hidden>Next undecided</button>
        </div>
      </div>
    </section>
    </div>
<Settings />
  </main>

</>;
}
