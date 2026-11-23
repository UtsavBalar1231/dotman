export type FileAction = "keep_local" | "take_repo" | "restore";

export interface EntryView {
  id: string;
  key: string;
  status: string;
  group: string;
  supported: boolean;
  system: boolean;
  held_actions?: ("add" | "plain" | "encrypt" | "forget")[];
  host_variant?: boolean;
  editable?: boolean;
  hold?: {
    origin: string;
    reason: string;
    rule: string | null;
    location: string | null;
    guidance: string | null;
  } | null;
  machine_file?: {
    selected: string | null;
    recorded: string | null;
    alternatives: string[];
    kind: string;
    identity_changed: boolean;
    ambiguous: boolean;
  } | null;
}

export interface MachineDetails {
  host: string;
  os: string;
  arch: string;
  profiles: string[];
}

interface ContextView {
  home: string;
  repo: string;
  config: string;
  state: string;
  identity: string;
  machine: string;
  machine_details?: MachineDetails;
}

interface StatusView {
  success: boolean;
  repo_unavailable: boolean;
  finding: "available" | "unavailable" | "in_progress" | "pull_conflict" | "stopped" | "unknown" | "companion_unavailable" | "invalid_report" | "timeout";
  message: string;
  context: ContextView | null;
  entries: EntryView[];
  key_stripped: boolean;
  clean: number;
  remote: string;
  held_counts?: { new_files: number; home_edits: number; repo_edits: number };
  identity_review_id?: string;
  hook_review_id?: string;
  setup_review_id?: string;
  key_review_id?: string;
  policy_review_id?: string;
}

interface DiffView {
  success: boolean;
  outcome: "ready" | "error" | "terminal_recovery";
  message: string;
  text: string | null;
  file_actions?: string[];
  line_edits?: LineEdit[];
  selected_push?: boolean;
}

export interface LineEdit {
  id: string;
  old_line: number | null;
  new_line: number | null;
  before: string | null;
  after: string | null;
}

export interface ReviewItem {
  group: string;
  text: string;
  diff_id?: string;
}

interface ReviewView {
  outcome: string;
  message: string;
  items: ReviewItem[];
  id: string | null;
  commit_message: string | null;
}

export type RecoveryRow =
  | { kind: "instruction"; text: string }
  | { kind: "command"; condition: string | null; shell: string };

interface WriterView {
  outcome: string;
  message: string;
  items: ReviewItem[];
  refresh: boolean;
  recovery?: RecoveryRow[];
}

export interface Channel {
  onmessage: (message: unknown) => void;
}

export type TrackingAction = "track_files" | "track_folder" | "stop_tracking";
export interface TrackingPreview {
  files: string[];
  folders: string[];
  held: Record<string,string>;
  existing: string[];
}

export interface Destination {
  branch: string | null;
  fetch: (string | null)[];
  push: (string | null)[];
  separate_push: boolean;
  editable: boolean;
  token: string;
}

export interface DestinationChange { fetch: string; push: string | null }

export interface Commands {
  sync_destination: { args: {change: DestinationChange | null; expected: string | null; dark: boolean}; result: Destination };
  open_workspace_folder: { args: {folder: "installation" | "repository"}; result: void };
  open_github_repository: { args: {repository: string}; result: void };
  preview_tracking: { args: {action: TrackingAction; key: string | null}; result: [string,TrackingPreview] | null };
  apply_tracking: { args: {id: string; dark: boolean}; result: boolean };
  open_source: { args: { id: string }; result: string };
  save_source: { args: { id: string; text: string }; result: void };
  refresh: { args: undefined; result: StatusView };
  cancel: { args: undefined; result: void };
  discard_review: { args: { id: string }; result: void };
  open_diff: { args: { id: string }; result: DiffView };
  open_review: { args: undefined; result: ReviewView };
  run_reviewed_sync: { args: { id: string; progress: Channel; dark?: boolean; commitMessage?: string; selectedLines?: string[]; massDelete?: boolean }; result: WriterView };
  apply_reviewed_files: { args: { answers: { id: string; action: FileAction }[] }; result: WriterView };
  accept_reviewed_identity: { args: { id: string }; result: WriterView };
  approve_pending_hooks: { args: { id: string }; result: WriterView };
  run_fresh_setup: { args: { id: string }; result: WriterView };
  run_clone_setup: { args: { id: string; source: string }; result: WriterView };
  run_import_setup: { args: { id: string; action: "import" }; result: WriterView };
  run_key: { args: { id: string; action: "create" | "import" }; result: WriterView };
  run_held: { args: { id: string; action: "add" | "plain" | "encrypt" | "forget" }; result: WriterView };
  run_host_variant: { args: { id: string }; result: WriterView };
  review_policy: { args: { id: string; action: "directories" | "ignores" }; result: WriterView };
}

export interface DesktopWindow {
  onCloseRequested(handler: (event: { preventDefault(): void }) => void): Promise<() => void>;
  destroy(): Promise<void>;
}

export interface TauriCore {
  invoke<K extends keyof Commands>(command: K, ...args: Commands[K]["args"] extends undefined
    ? [] : [args: Commands[K]["args"]]): Promise<Commands[K]["result"]>;
  Channel: new () => Channel;
}
