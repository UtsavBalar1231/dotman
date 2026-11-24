import type { TauriCore } from "./protocol.js";

export function policyControls(options: {
  invoke: TauriCore["invoke"];
  getId: () => string | null;
  pending: (action: "directories" | "ignores") => number | undefined;
  canStart: () => boolean;
  begin: () => void;
  finish: (refresh: boolean) => Promise<void>;
}) {
  const message = document.getElementById("policy-message");
  if (!message) throw new Error("Missing policy message");
  for (const action of ["directories", "ignores"] as const) {
    const button = document.getElementById(`policy-${action}`);
    if (!button) throw new Error("Missing policy control");
    button.addEventListener("click", async () => {
      const id = options.getId();
      if (!id || !options.canStart()) return;
      if (options.pending(action) === 0) {
        message.textContent = action === "directories"
          ? "No directories are waiting for capture approval."
          : "No retained ignore rules are waiting for release.";
        return;
      }
      options.begin();
      message.textContent = "Checking the capture policy…";
      let refresh = false;
      try {
        const result = await options.invoke("review_policy", { id, action });
        message.textContent = result.message;
        refresh = result.refresh === true && result.outcome !== "cleanup_unresolved" && result.outcome !== "terminal_recovery";
      } catch {
        message.textContent = "The policy review was interrupted. Refresh status before reviewing it again.";
        refresh = true;
      } finally {
        await options.finish(refresh);
      }
    });
  }
}
