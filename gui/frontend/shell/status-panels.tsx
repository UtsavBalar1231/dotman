import { Icon } from "./symbols.js";

export function StatusPanels() {
return <>
    <section id="recovery" hidden aria-labelledby="recovery-title">
      <h2 id="recovery-title" className="with-glyph"><Icon name="warning" />Repository troubleshooting</h2>
      <p id="recovery-message"></p>
      <p id="recovery-finding"></p>
      <p>Recovery is not available in this panel. A fresh check only reads status; it does not resolve a conflict or resume sync.</p>
      <button id="recovery-check" type="button">Check status again</button>
      <details><summary><Icon name="chevron-right" className="disclosure-glyph" />Resolved paths</summary><dl id="recovery-paths"></dl></details>
      <details id="recovery-details" hidden><summary><Icon name="chevron-right" className="disclosure-glyph" />Terminal recovery details</summary><div id="recovery-steps"></div></details>
    </section>
    <section id="setup" hidden aria-labelledby="setup-title">
      <h2 id="setup-title">Repository unavailable</h2>
      <p id="setup-finding"></p>
      <button id="setup-check" type="button">Check status again</button>
      <p id="setup-guidance"></p>
      <button id="start-fresh" type="button" hidden>Create repository…</button>
      <button id="start-import" type="button" hidden>Import dotfiles directory…</button>
      <div id="clone-controls" hidden>
        <label htmlFor="clone-source">Clone source</label>
        <p id="clone-help">Use a public HTTPS URL without credentials, or an absolute file URL (Git 2.45.1 or newer). Clone puts files in the repository for separate review.</p>
        <input id="clone-source" type="text" maxLength={8192} autoComplete="off" autoCapitalize="none" spellCheck="false" aria-describedby="clone-help" />
        <button id="start-clone" type="button">Clone repository…</button>
      </div>
      <details><summary><Icon name="chevron-right" className="disclosure-glyph" />Resolved paths</summary><dl id="setup-paths"></dl></details>
    </section>
    <section id="setup-result" hidden aria-labelledby="setup-result-title">
      <h2 id="setup-result-title">Repository setup result</h2>
      <p id="setup-message" role="status" aria-live="polite" tabIndex={-1}></p>
      <button id="setup-retry" type="button" hidden>Check status again</button>
    </section>
    <section id="decisions" hidden aria-labelledby="decisions-title">
      <div className="section-head"><h2 id="decisions-title" tabIndex={-1}>File decisions</h2><button id="close-decisions" type="button">Close file decisions</button></div>
      <p id="decision-message" role="status" aria-live="polite" tabIndex={-1}></p>
      <ul id="decision-items"></ul>
      <div className="actions"><button id="clear-decisions">Clear all</button><button id="retry-decisions" type="button" hidden>Refresh and review again</button><button id="apply-decisions">Apply file decisions</button></div>
      <div id="decision-results"></div>
    </section>
    <section id="identity-result" hidden aria-labelledby="identity-result-title">
      <h2 id="identity-result-title">Identity review result</h2>
      <p id="identity-message" role="status" aria-live="polite" tabIndex={-1}></p>
    </section>
    <section id="variant-result" hidden aria-labelledby="variant-result-title">
      <h2 id="variant-result-title">Host variant result</h2>
      <p id="variant-message" role="status" aria-live="polite" tabIndex={-1}></p>
    </section>
    <section id="hooks-result" hidden aria-labelledby="hooks-result-title">
      <h2 id="hooks-result-title">Hook review result</h2>
      <p id="hooks-message" role="status" aria-live="polite" tabIndex={-1}></p>
    </section>

</>;
}
