export type Page = "files" | "review" | "settings";

type WorkspaceView = {
  readonly page: Page;
  readonly repository: string;
  readonly status: string;
  readonly busy: boolean;
  readonly cancel: string | null;
  readonly queued: string | null;
  readonly cancelling: boolean;
};

let view: WorkspaceView = {
  page: "files",
  repository: "",
  status: "Loading status…",
  busy: false,
  cancel: null,
  queued: null,
  cancelling: false,
};
const subscribers = new Set<() => void>();

export function getWorkspaceView(): WorkspaceView {
  return view;
}

export function subscribeToWorkspace(listener: () => void): () => void {
  subscribers.add(listener);
  return () => { subscribers.delete(listener); };
}

export function updateWorkspace(change: Partial<WorkspaceView>): void {
  const next = { ...view, ...change };
  if (next.page === view.page && next.repository === view.repository && next.status === view.status && next.busy === view.busy &&
      next.cancel === view.cancel && next.queued === view.queued && next.cancelling === view.cancelling) return;
  view = next;
  for (const notify of subscribers) notify();
}
