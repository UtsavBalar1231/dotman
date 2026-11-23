import { useRef, useState } from "react";
import type { Destination } from "../protocol.js";
import { Icon } from "./symbols.js";

function githubRepository(url: string) {
  const address = url.toLowerCase();
  const prefix = [
    "https://github.com/", "https://github.com:443/", "git@github.com:",
    "ssh://git@github.com/", "ssh://git@github.com:22/", "ssh://git@ssh.github.com:443/",
  ].find(value => address.startsWith(value));
  if (!prefix) return;
  const path = url.slice(prefix.length).replace(/\/$/, "").replace(/\.git$/i, "");
  if (/^[\w.-]+\/[\w.-]+$/.test(path)) return path;
}

export function DestinationSettings() {
  const [destination, setDestination] = useState<Destination | null>(null);
  const [editing, edit] = useState(false);
  const [pending, wait] = useState(false);
  const [message, report] = useState("");
  const [fetchUrl, setFetch] = useState("");
  const [pushUrl, setPush] = useState("");
  const [override, setOverride] = useState(false);
  const editButton = useRef<HTMLButtonElement>(null);

  async function load(save = false) {
    if (pending || (save && !destination)) return;
    wait(true);
    report(save ? "Waiting for confirmation…" : "Reading origin…");
    try {
      const { destinationOperation } = await import("../app.js");
      const next = await destinationOperation(save ? {
        fetch: fetchUrl.trim(), push: override ? pushUrl.trim() : null,
      } : null, save ? destination!.token : null);
      setDestination(next);
      edit(false);
      report(save ? "Destination saved." : "");
      if (save) requestAnimationFrame(() => editButton.current?.focus());
    } catch (error) {
      if (!save) setDestination(null);
      report(String(error instanceof Error ? error.message : error));
    } finally { wait(false); }
  }

  function begin() {
    if (!destination) return;
    setFetch(destination.fetch[0] ?? "");
    setOverride(destination.separate_push);
    setPush(destination.push[0] ?? "");
    report("");
    edit(true);
  }

  const addresses = (values: (string | null)[]) => values.map((url,index) => {
    const repository = url && githubRepository(url);
    return <dd key={index}><span>{url ?? "Private or unsupported address"}</span>
      {repository && <button type="button" className="destination-link"
        aria-label={`Open ${repository} on GitHub`}
        onClick={() => void window.__TAURI__.core.invoke("open_github_repository", { repository })
          .catch(() => report("Could not open the default browser."))}>Open on GitHub ↗</button>}</dd>;
  });
  const shared = destination && destination.fetch.length === 1 && destination.fetch[0] &&
    destination.push.length === 1 && destination.push[0] === destination.fetch[0];
  const changed = destination && (fetchUrl.trim() !== (destination.fetch[0] ?? "") ||
    override !== destination.separate_push || (override && pushUrl.trim() !== destination.push[0]));

  return <details id="destination-settings" onToggle={e => {
    if (e.currentTarget.open && !editing) void load();
  }}>
    <summary><Icon name="chevron-right" className="disclosure-glyph" />Sync destination</summary>
    {destination && !editing && <>
      {destination.fetch.length || destination.push.length ? <dl className="destination-addresses">
        <dt>{shared ? "Fetch and push · origin" : "Fetch from · origin"}</dt>{addresses(destination.fetch)}
        {!shared && <><dt>Push to · origin</dt>{addresses(destination.push)}</>}
        <dt>Branch</dt><dd>{destination.branch ?? "Detached HEAD"}</dd>
      </dl> : <p>This repository syncs locally. Add an origin to use a remote.</p>}
      {destination.editable ? <button ref={editButton} type="button" disabled={pending} onClick={begin}>
        {destination.fetch.length ? "Edit destination…" : "Add destination…"}</button>
        : <p>This origin uses private addresses or an advanced Git configuration. Its destinations are shown read-only.</p>}
    </>}
    {editing && <form className="destination-form" onSubmit={e => {e.preventDefault(); void load(true);}}>
      <label>Repository URL<input autoFocus value={fetchUrl} onChange={e => setFetch(e.target.value)} maxLength={2048}
        required disabled={pending} spellCheck={false} autoComplete="off" placeholder="git@github.com:owner/dotfiles.git" /></label>
      <label className="destination-override"><input type="checkbox" disabled={pending} checked={override}
        onChange={e => setOverride(e.target.checked)} />Use a different URL for pushes</label>
      {override && <label>Push URL<input value={pushUrl} onChange={e => setPush(e.target.value)} required
        maxLength={2048} disabled={pending} autoComplete="off" spellCheck={false} /></label>}
      <div className="actions"><button type="button" disabled={pending} onClick={() => {edit(false); report(""); requestAnimationFrame(() => editButton.current?.focus());}}>Cancel</button>
        <button type="submit" disabled={pending || !changed || !fetchUrl.trim() || (override && !pushUrl.trim())}>Review change…</button></div>
    </form>}
    {message && <p role="status">{message}</p>}
    {!pending && (!destination || (editing && message)) && <button type="button" onClick={() => void load()}>Reload destination</button>}
  </details>;
}
