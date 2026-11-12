#!/usr/bin/env bash
# e2e/distros/run.sh [IMAGE...]: runs the journeys in one container per distro, as uid 1000, against a static musl dotman.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
e2e=$(dirname "$here")
root=$(dirname "$e2e")
images=("$@")
[[ ${#images[@]} -gt 0 ]] || images=(ubuntu:24.04 debian:13 fedora:latest archlinux:latest alpine:latest ubuntu:20.04)
command -v docker >/dev/null || {
    echo "e2e: docker is not on PATH" >&2
    exit 2
}

bin=${DOTMAN_BIN_MUSL:-}
if [[ -z $bin ]]; then
    (cd "$root" && cargo build --release --locked --target x86_64-unknown-linux-musl)
    bin="$(realpath "${CARGO_TARGET_DIR:-$root/target}")/x86_64-unknown-linux-musl/release/dotman"
fi
bin=$(realpath -- "$bin")
[[ -x $bin ]] || {
    echo "e2e: $bin is not an executable" >&2
    exit 2
}

out=$(mktemp -d)
trap 'rm -rf -- "$out"' EXIT

# Runs as root in the container: installs what the harness needs, then the journeys as uid 1000.
# ubuntu:20.04 has no age package, so it takes age's release tarball; its git is 2.25, the floor.
# shellcheck disable=SC2016  # the container's sh expands it
setup='
set -eu
. /etc/os-release
case $ID in
    ubuntu | debian)
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq git bash util-linux ca-certificates >/dev/null
        if ! apt-get install -y -qq age >/dev/null 2>&1; then
            apt-get install -y -qq curl >/dev/null
            curl -fsSL https://github.com/FiloSottile/age/releases/download/v1.2.1/age-v1.2.1-linux-amd64.tar.gz |
                tar -xz -C /usr/local/bin --strip-components=1 age/age age/age-keygen
        fi
        ;;
    fedora) dnf install -y -q git age bash util-linux diffutils findutils >/dev/null ;;
    arch) pacman -Sy --noconfirm --needed git age bash util-linux diffutils findutils >/dev/null ;;
    alpine) apk add --no-cache -q git age bash util-linux coreutils findutils grep sed diffutils >/dev/null ;;
    *) echo "no install recipe for $ID" >&2; exit 1 ;;
esac
user=$(getent passwd 1000 | cut -d: -f1 || true)
if [ -z "$user" ]; then
    user=e2e
    if command -v useradd >/dev/null; then useradd -m -u 1000 "$user"; else adduser -D -u 1000 "$user"; fi
fi
echo "distro: $PRETTY_NAME; $(git --version); age $(age --version)"
# su gives a user umask 002 on Debian and Ubuntu; the journeys expect 022, as on the CI runner,
# where a group-writable new directory would add a [modes] line to an edit 11-ignore records.
su -s /bin/bash -c "umask 022 && cd /tmp && DOTMAN_BIN=/opt/dotman/dotman bash /opt/e2e/run.sh journeys" "$user"
'

pids=()
for image in "${images[@]}"; do
    log=$out/${image//[:\/]/_}.log
    docker run --rm --network bridge \
        -v "$e2e:/opt/e2e:ro" -v "$bin:/opt/dotman/dotman:ro" \
        "$image" sh -c "$setup" >"$log" 2>&1 &
    pids+=($!)
done
status=0
for pid in "${pids[@]}"; do wait "$pid" || status=1; done

# One row per journey, one column per image: pass, FAIL, or ? when the run never got that far.
mapfile -t scenarios < <(cd "$e2e" && for f in journeys/*.sh; do printf '%s\n' "${f%.sh}"; done)
printf '\n%-32s' scenario
for image in "${images[@]}"; do printf ' %-16s' "$image"; done
printf '\n'
for scenario in "${scenarios[@]}"; do
    printf '%-32s' "$scenario"
    for image in "${images[@]}"; do
        log=$out/${image//[:\/]/_}.log
        if grep -qxF "failed: $scenario" "$log"; then
            cell=FAIL
        elif grep -qE "^$scenario +[0-9]+ ms$" "$log"; then
            cell=pass
        else
            cell='?'
        fi
        [[ $cell == pass ]] || status=1
        printf ' %-16s' "$cell"
    done
    printf '\n'
done
printf '\n'
for image in "${images[@]}"; do
    log=$out/${image//[:\/]/_}.log
    printf '%s: %s\n' "$image" "$(grep -m1 '^distro: ' "$log" || echo 'setup failed')"
    grep -E '^(FAIL|scenarios:|assertions:|invariant violations:)' "$log" | sed 's/^/  /' || true
    if ! grep -q '^scenarios:' "$log"; then tail -n 20 "$log" | sed 's/^/  | /'; fi
done
exit "$status"
