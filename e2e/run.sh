#!/usr/bin/env bash
# e2e/run.sh [SUITE | SCENARIO]...: runs every suite, or those named, against $DOTMAN_BIN.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
[[ -n ${DOTMAN_BIN:-} ]] || {
    echo "e2e: set DOTMAN_BIN to a dotman binary, or run 'just e2e'" >&2
    exit 2
}
[[ -x $DOTMAN_BIN ]] || {
    echo "e2e: DOTMAN_BIN=$DOTMAN_BIN is not an executable" >&2
    exit 2
}
DOTMAN_BIN=$(realpath -- "$DOTMAN_BIN")
export DOTMAN_BIN

scenarios=()
add_suite() {
    local file
    for file in "$1"/*.sh; do
        [[ -e $file ]] && scenarios+=("$file")
    done
}
if [[ $# == 0 ]]; then
    # distros/ is the container matrix that `just e2e-distros` runs, not a suite.
    for dir in "$here"/*/; do [[ $dir == */distros/ ]] || add_suite "${dir%/}"; done
else
    for arg in "$@"; do
        if [[ -d $here/$arg ]]; then
            add_suite "$here/$arg"
        elif [[ -f $arg ]]; then
            scenarios+=("$(realpath -- "$arg")")
        else
            match=("$here"/*/"${arg%.sh}".sh)
            [[ -f ${match[0]} ]] || {
                echo "e2e: no suite or scenario named '$arg'" >&2
                exit 2
            }
            scenarios+=("${match[@]}")
        fi
    done
fi
[[ ${#scenarios[@]} -gt 0 ]] || {
    echo "e2e: no scenarios found" >&2
    exit 2
}

results=$(mktemp)
trap 'rm -f -- "$results"' EXIT
export E2E_RESULTS=$results

timings=()
failed=()
start=$SECONDS
for file in "${scenarios[@]}"; do
    name="$(basename "$(dirname "$file")")/$(basename "$file" .sh)"
    began=$(date +%s%N)
    if ! bash "$file"; then
        failed+=("$name")
    fi
    ended=$(date +%s%N)
    timings+=("$(printf '%-36s %6d ms' "$name" $(((ended - began) / 1000000)))")
done

passed=0 fails=0 violations=0
while IFS=$'\t' read -r _ p f v; do
    passed=$((passed + p))
    fails=$((fails + f))
    violations=$((violations + v))
done <"$results"

printf '\n=== summary\n'
printf '%s\n' "${timings[@]}"
printf 'scenarios: %d run, %d failed, in %ds\n' "${#scenarios[@]}" "${#failed[@]}" $((SECONDS - start))
printf 'assertions: %d passed, %d failed\n' "$passed" "$fails"
printf 'invariant violations: %d\n' "$violations"
if [[ ${#failed[@]} -gt 0 ]]; then
    printf 'failed: %s\n' "${failed[@]}"
    exit 1
fi
