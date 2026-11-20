import type { TauriCore } from "./protocol.js";

export function runKey(invoke: TauriCore["invoke"], id: string, action: "create" | "import") {
  return invoke("run_key", { id, action });
}
