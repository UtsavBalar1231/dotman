import { Icon } from "./symbols.js";

export function TreeTracking() {
  return <div className="tree-heading">
    <h2 id="files-title">File browser</h2>
    <details className="track-menu" id="track-menu">
      <summary title="Track files or folders" aria-label="Tracking actions">Track <Icon name="chevron-down" /></summary>
      <div className="track-options">
        <button id="track-files" type="button">Choose files…</button>
        <button id="track-folder" type="button">Choose folder…</button>
        <div className="track-removal"><button id="untrack-folder" type="button">Stop tracking a folder…</button></div>
      </div>
    </details>
  </div>;
}

export function TrackingDrawer() {
  return <dialog id="tracking-drawer" aria-labelledby="tracking-title" aria-describedby="tracking-scope">
    <header><h2 id="tracking-title">Review tracking</h2>
      <button type="button" id="tracking-close" title="Close tracking review" aria-label="Close tracking review"><Icon name="close" /></button></header>
    <section className="tracking-body">
      <p id="tracking-location" hidden><Icon name="folder" /><bdi id="tracking-root" /></p>
      <p id="tracking-scope"></p>
      <label id="tracking-future" hidden><input id="tracking-future-input" type="checkbox" />Track future files in this folder</label>
      <div id="tracking-groups"></div>
      <p id="tracking-message" role="status" aria-live="polite"></p>
    </section>
    <footer><span className="tracking-locality"><Icon name="desktop" /><span id="tracking-count-label" role="status">Local change</span></span><button type="button" id="tracking-apply" className="review-action">Track selection</button></footer>
  </dialog>;
}
