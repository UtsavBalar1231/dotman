#!/bin/sh
set -eu

: "${DOTMAN_SSH_FIXTURE_IMAGE:=act-runner-rust:local}"
export DOTMAN_SSH_FIXTURE_IMAGE

log=$(mktemp "${TMPDIR:-/tmp}/dotman-transport.XXXXXX")
trap 'rm -f "$log"' EXIT HUP INT TERM
run() {
    cargo test --test cli_selected "$1" -- --ignored --exact --nocapture >"$log" 2>&1 || {
        cat "$log"
        exit 1
    }
    cat "$log"
    grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$log"
}
run transport::selected_https_verified_loopback_success_and_divergence
run transport::selected_https_uses_only_exact_private_cache_credentials
run transport::selected_ssh_verified_loopback_success_and_divergence
