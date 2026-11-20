export function policyControls(options) {
    const message = document.getElementById("policy-message");
    if (!message)
        throw new Error("Missing policy message");
    for (const action of ["directories", "ignores"]) {
        const button = document.getElementById(`policy-${action}`);
        if (!button)
            throw new Error("Missing policy control");
        button.addEventListener("click", async () => {
            const id = options.getId();
            if (!id || !options.canStart())
                return;
            options.begin();
            message.textContent = "Reviewing local policy; native confirmation follows.";
            let refresh = false;
            try {
                const result = await options.invoke("review_policy", { id, action });
                message.textContent = `${result.outcome}: ${result.message}`;
                refresh = result.refresh === true && result.outcome !== "cleanup_unresolved" && result.outcome !== "terminal_recovery";
            }
            catch {
                message.textContent = "Policy review failed. Check status in a terminal before continuing.";
            }
            finally {
                await options.finish(refresh);
            }
        });
    }
}
