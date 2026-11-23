const statusLabels = new Map([
  ["both-changed", "Both copies changed"],
  ["changed-in-repo", "Repository changed"],
  ["changed-locally", "Local change"],
  ["clean", "In sync"],
  ["new", "New file"],
  ["not-applied", "Ready to apply"],
  ["held-in-home", "Held on this computer"],
  ["held-in-repo", "Held in repository"],
]);

export function displayStatus(status: string): string {
  const readable = status.replaceAll("-", " ");
  return statusLabels.get(status) ?? readable.charAt(0).toLocaleUpperCase() + readable.slice(1);
}

export function matchesSearch(entry: { key: string; status: string }, query: string) {
  return !query || entry.key.toLocaleLowerCase().includes(query)
    || entry.status.toLocaleLowerCase().includes(query)
    || displayStatus(entry.status).toLocaleLowerCase().includes(query);
}
