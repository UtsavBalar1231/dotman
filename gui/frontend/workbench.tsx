import { TreeTracking, TrackingDrawer } from "./shell/tracking.js";
import { TreeDivider } from "./shell/tree-divider.js";
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
    <dialog id="discard-review-work" aria-labelledby="discard-work-title" aria-describedby="discard-work-description">
      <h2 id="discard-work-title"></h2>
      <p id="discard-work-description"></p>
      <div className="actions">
        <button id="keep-review-work" type="button">Keep reviewing</button>
        <button id="discard-review-work-confirm" type="button"></button>
      </div>
    </dialog>
    <div className="desk">
    <section id="entries" hidden aria-labelledby="files-title">
      <TreeTracking />
      <div className="queue-meta"><button id="open-destination" type="button" disabled title="View or change sync destination">
        <Icon name="repository" /><span id="remote"></span><Icon name="chevron-right" /></button><span id="clean"></span></div>
      <button id="review-hooks" type="button" hidden>Review pending hooks</button>
      <div className="search-heading"><label htmlFor="search">Search files</label><span id="search-count" role="status"></span></div>
      <div className="file-search">
        <Icon name="search" />
        <input id="search" type="text" role="searchbox" inputMode="search" placeholder="Name, path or status" autoComplete="off" aria-keyshortcuts="Control+f" title="Search files (Ctrl+F)" />
        <button id="clear-search" type="button" aria-label="Clear search" title="Clear search (Esc)" hidden><Icon name="close" /></button>
      </div>
      <div className="nav-modes" role="group" aria-label="Group files">
        <button id="group-folders" className="with-glyph" aria-pressed="true"><Icon name="folder" />All files</button>
        <button id="group-status" className="with-glyph" aria-pressed="false"><Icon name="changed" />Changed</button>
      </div>
      <p id="held-counts" hidden></p>
      <div id="rows" tabIndex={0} aria-label="File change queue"></div>
      <TreeDivider />
     </section>
     <section id="comparison-guide" className="desk-guide" aria-labelledby="guide-title">
        <h2 id="guide-title">File comparison</h2>
        <p id="guide-description"></p>
        <ul id="attention-list" className="attention-list"></ul>
        <button id="more-comparisons" type="button" aria-controls="attention-list" hidden></button>
     </section>
    <dialog id="review" hidden aria-labelledby="review-title">
      <div className="section-head">
        <span className="sync-emblem" id="review-icon" aria-hidden="true"><Icon name="repository" /></span>
        <div className="sync-heading"><h2 id="review-title" aria-live="polite">Outgoing sync</h2><p id="review-remote"></p></div><button id="close-review" className="icon-button" aria-label="Close review" title="Close review"><Icon name="close" /></button></div>
      <div id="review-body">
      <p id="review-message" role="status" aria-live="polite" tabIndex={-1}></p>
      <details id="commit-editor" hidden>
        <summary title="View or edit the commit message"><Icon name="chevron-right" className="disclosure-glyph" />
          <span>Commit message</span><span id="commit-preview"></span></summary>
        <p id="commit-hint">Used if sync creates a commit. Up to six lines and 1,024 UTF-8 bytes.</p>
        <textarea id="commit-message" aria-label="Commit message" rows={4} aria-describedby="commit-hint commit-error"></textarea>
        <p id="commit-error" role="status" aria-live="polite"></p>
        <button id="restore-commit-message" type="button" hidden>Use suggested message</button>
      </details>
      <div id="review-items"></div>
      <div id="line-summary" hidden><p id="selected-effect"></p><ul id="selected-lines"></ul><button id="leave-lines" type="button">Leave selected-only mode</button></div>
      </div>
      <div className="review-action-strip">
      <p id="review-footnote" hidden>Hooks require separate approval.</p>
      <button id="review-return" hidden>Back to files</button>
      <button id="cancel-review" hidden>Cancel sync</button>
      <button id="sync-reviewed" hidden>Continue to confirmation</button>
      <button id="sync-mass-delete" type="button" hidden>Review and approve mass deletion</button>
      <button id="sync-lines" type="button" hidden>Commit selected lines</button>
      </div>
    </dialog>
    <section id="diff" hidden aria-labelledby="diff-key">
      <div id="comparison-body">
        <div className="section-head">
          <div className="comparison-title" role="group" aria-label="File and status" tabIndex={0}>
            <h2 id="diff-key"></h2>
            <p id="diff-context"></p>
          </div>
          <div id="source-edit-details"></div>
          <button id="close-diff" className="with-glyph" type="button">Back to files</button>
        </div>
        <div className="comparison-content">
          <p id="diff-message" role="status" aria-live="polite"></p>
          <div className="comparison-reader">
            <div id="reader-tools" hidden></div>
            <pre id="diff-text" tabIndex={0} aria-label="Diff text"></pre>
          </div>
          <fieldset id="line-edits" hidden><legend>Choose changes to commit</legend><p>Checking a change enters selected-only mode. Replacements include both sides.</p><div id="line-edit-list"></div></fieldset>
        </div>
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
          <div id="tracking-file-action"></div><div id="diff-actions" className="actions"></div>
          <button id="next-undecided" hidden>Next undecided</button>
        </div>
      </div>
    </section>
    </div>
<Settings />
    <TrackingDrawer />
    <dialog id="source-editor" aria-labelledby="editor-key">
      <header className="source-heading">
        <div className="source-location">
          <p><Icon name="repository" /><span id="editor-directory"></span></p>
          <h2 id="editor-key"></h2>
        </div>
        <button className="source-save" title="Save repository source (Ctrl+S)">Save source</button>
        <button className="source-cancel icon-button" aria-label="Close editor" title="Close editor (Escape)"><Icon name="close" /></button>
      </header>
      <fieldset className="source-tools" aria-label="Editing commands" disabled>
        <button className="source-find" title="Find and replace (Ctrl+F)"><Icon name="search" />Find</button>
        <button className="source-undo" title="Undo (Ctrl+Z)">Undo</button>
        <button className="source-redo" title="Redo (Ctrl+Shift+Z)">Redo</button>
        <button className="source-wrap" aria-pressed="false" title="Wrap long lines">Wrap lines</button>
      </fieldset>
      <div id="source-buffer"></div>
      <div className="source-notice"><p role="status" aria-live="polite"></p>
        <button className="source-copy" title="Copy the full unsaved source to the clipboard" hidden>Copy draft</button>
        <div className="source-exit" role="group" aria-label="Unsaved edits" hidden>
        <button className="source-continue">Keep editing</button><button className="source-save-exit">Save and close…</button><button className="source-discard">Discard edits</button>
      </div></div>
      <footer className="source-status">
        <span className="source-language"></span>
        <span className="source-eol"></span>
        <select className="source-indent" aria-label="Indentation" title="Indentation for new edits" disabled>
          <option value="2">2 spaces</option><option value="4">4 spaces</option><option value="tab">Tabs</option>
        </select>
        <button className="source-position" title="Go to line (Ctrl+Alt+G)" aria-label="Go to line" disabled></button>
      </footer>
    </dialog>
  </main>

</>;
}
