export function runKey(invoke, id, action) {
    return invoke("run_key", { id, action });
}
