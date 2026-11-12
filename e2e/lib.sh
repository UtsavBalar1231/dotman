#!/usr/bin/env bash
# e2e/lib.sh: the harness every e2e scenario sources, with its machines, runners and invariants.

# shellcheck disable=SC2034  # E2E_RC, E2E_OUT and E2E_ERR are read by the scenarios.

E2E_ROOT=
E2E_NAME=
E2E_PATH=
E2E_PASS=0
E2E_FAIL=0
E2E_VIOLATIONS=0
E2E_FINISHED=0
E2E_RC=0
E2E_OUT=
E2E_ERR=
E2E_LAST_CMD=
E2E_LAST_MACHINE=
E2E_PRINTED_RC=0
E2E_MACHINES=()

# Prints a message to stderr and exits; for harness misuse, not for product failures.
e2e_die() {
    printf 'e2e: %s\n' "$*" >&2
    exit 2
}

# Resolves a path that may not exist yet, following the symlinks of its existing parents.
e2e_resolve() {
    realpath -m -- "$1"
}

# Starts scenario NAME: checks DOTMAN_BIN, creates the temp root, arms the cleanup trap and
# builds PATH from git, age, age-keygen, bash and any further TOOLS named after NAME.
e2e_begin() {
    E2E_NAME=$1
    shift
    [[ -n ${DOTMAN_BIN:-} ]] || e2e_die "DOTMAN_BIN is not set; run 'just e2e'"
    [[ -x $DOTMAN_BIN ]] || e2e_die "DOTMAN_BIN=$DOTMAN_BIN is not an executable"
    DOTMAN_BIN=$(e2e_resolve "$DOTMAN_BIN")
    local base
    base=$(e2e_resolve "${TMPDIR:-/tmp}")
    E2E_ROOT=$(mktemp -d "$base/dotman-e2e.XXXXXX")
    E2E_ROOT=$(e2e_resolve "$E2E_ROOT")
    case $E2E_ROOT in
        "" | / | "$base") e2e_die "refusing the temp root '$E2E_ROOT'" ;;
    esac
    trap e2e_on_exit EXIT
    mkdir -p "$E2E_ROOT/bin" "$E2E_ROOT/ledger" "$E2E_ROOT/tool/home"
    : >"$E2E_ROOT/ledger/edits"
    : >"$E2E_ROOT/ledger/modes"
    : >"$E2E_ROOT/ledger/identities"
    : >"$E2E_ROOT/log"
    : >"$E2E_ROOT/last.out"
    : >"$E2E_ROOT/last.err"
    ln -s "$DOTMAN_BIN" "$E2E_ROOT/bin/dotman"
    # A real gh must never run: no network, and no account the developer is signed in to.
    printf '#!/bin/sh\necho "real gh blocked in e2e" >&2\nexit 97\n' >"$E2E_ROOT/bin/gh"
    chmod 0755 "$E2E_ROOT/bin/gh"
    printf '[user]\n\tname = e2e tool\n\temail = tool@example.invalid\n[init]\n\tdefaultBranch = main\n' \
        >"$E2E_ROOT/tool/home/.gitconfig"
    E2E_PATH="$E2E_ROOT/bin"
    local tool found
    for tool in git age age-keygen bash "$@"; do
        found=$(command -v "$tool") || e2e_die "$tool is not on PATH"
        found=$(dirname "$found")
        case ":$E2E_PATH:" in *":$found:"*) ;; *) E2E_PATH="$E2E_PATH:$found" ;; esac
    done
    E2E_PATH="$E2E_PATH:/usr/local/bin:/usr/bin:/bin"
    printf '=== %s\n' "$E2E_NAME"
}

# The EXIT trap: an unfinished scenario counts as a failure; removes the root unless E2E_KEEP=1.
e2e_on_exit() {
    local rc=$?
    if [[ $E2E_FINISHED != 1 ]]; then
        printf 'FAIL  [%s] the scenario stopped before e2e_finish (exit %s)\n' "$E2E_NAME" "$rc"
        E2E_FAIL=$((E2E_FAIL + 1))
        e2e_record
    fi
    if [[ ${E2E_KEEP:-0} == 1 ]]; then
        printf 'kept  [%s] %s\n' "$E2E_NAME" "$E2E_ROOT"
    elif [[ -n $E2E_ROOT && -d $E2E_ROOT ]]; then
        # git writes its objects read-only.
        chmod -R u+rwX "$E2E_ROOT" 2>/dev/null || true
        rm -rf -- "$E2E_ROOT"
    fi
}

# Appends this scenario's counts to $E2E_RESULTS, the file run.sh summarises.
e2e_record() {
    [[ -n ${E2E_RESULTS:-} ]] || return 0
    printf '%s\t%s\t%s\t%s\n' "$E2E_NAME" "$E2E_PASS" "$E2E_FAIL" "$E2E_VIOLATIONS" >>"$E2E_RESULTS"
}

# Machine M's root, HOME, repo, state dir, machine config dir, backups dir and private bin dir.
e2e_mroot() { printf '%s/m/%s' "$E2E_ROOT" "$1"; }
e2e_home() { printf '%s/home' "$(e2e_mroot "$1")"; }
e2e_repo() { printf '%s/xdg/data/dotman' "$(e2e_mroot "$1")"; }
e2e_state() { printf '%s/xdg/state/dotman' "$(e2e_mroot "$1")"; }
e2e_config() { printf '%s/xdg/config/dotman' "$(e2e_mroot "$1")"; }
e2e_backups() { printf '%s/backups' "$(e2e_state "$1")"; }
e2e_bin() { printf '%s/bin' "$(e2e_mroot "$1")"; }

# The shared bare remote, as a path and as its file:// URL.
e2e_remote() { printf '%s/remote.git' "$E2E_ROOT"; }
e2e_remote_url() { printf 'file://%s' "$(e2e_remote)"; }

# Creates the shared bare remote, empty, on branch main.
e2e_make_remote() {
    e2e_tool git init -q --bare --template= --initial-branch=main "$(e2e_remote)" >/dev/null
}

# Creates machine M with its own HOME, XDG dirs, bin dir and git identity "Machine M".
e2e_machine() {
    local m=$1 r
    r=$(e2e_mroot "$m")
    mkdir -p "$r/home" "$r/xdg/config" "$r/xdg/state" "$r/xdg/data" "$r/xdg/cache" "$r/run" "$r/tmp" "$r/bin"
    chmod 0700 "$r/run"
    printf '[user]\n\tname = Machine %s\n\temail = %s@example.invalid\n[init]\n\tdefaultBranch = main\n' \
        "${m^^}" "$m" >"$r/home/.gitconfig"
    E2E_MACHINES+=("$m")
}

# Creates the shared remote when it does not exist yet, and one machine per argument.
e2e_machines() {
    [[ -d $(e2e_remote) ]] || e2e_make_remote
    local m
    for m in "$@"; do e2e_machine "$m"; done
}

# Writes an executable NAME with BODY into machine M's bin dir, which comes first on its PATH.
e2e_stub() {
    printf '%s' "$3" >"$(e2e_bin "$1")/$2"
    chmod 0755 "$(e2e_bin "$1")/$2"
}

# Refuses to go on unless HOME and every XDG dir of machine M are inside the temp root and
# none of them is the real home.
e2e_guard() {
    local m=$1 path real_home caller_home
    [[ -n $E2E_ROOT && -d $E2E_ROOT ]] || e2e_die "no temp root; call e2e_begin first"
    [[ -d $(e2e_mroot "$m") ]] || e2e_die "no machine '$m'; call e2e_machines first"
    real_home=$(e2e_resolve "$(getent passwd "$(id -u)" | cut -d: -f6)")
    caller_home=$(e2e_resolve "${HOME:-/}")
    for path in home xdg/config xdg/state xdg/data xdg/cache; do
        path=$(e2e_resolve "$(e2e_mroot "$m")/$path")
        case $path in
            "$E2E_ROOT"/*) ;;
            *) e2e_die "machine $m: $path is outside the temp root $E2E_ROOT" ;;
        esac
        [[ $path != "$real_home" && $path != "$caller_home" ]] || e2e_die "machine $m: $path is the real home"
    done
}

# Runs a command in machine M's environment, from its HOME, under env -i.
e2e_on() {
    local m=$1 r
    shift
    e2e_guard "$m"
    r=$(e2e_mroot "$m")
    (
        cd "$r/home" &&
            exec env -i \
                HOME="$r/home" \
                USER="e2e-$m" \
                XDG_CONFIG_HOME="$r/xdg/config" \
                XDG_STATE_HOME="$r/xdg/state" \
                XDG_DATA_HOME="$r/xdg/data" \
                XDG_CACHE_HOME="$r/xdg/cache" \
                XDG_RUNTIME_DIR="$r/run" \
                TMPDIR="$r/tmp" \
                PATH="$r/bin:$E2E_PATH" \
                LC_ALL=C \
                NO_PAGER=1 \
                DOTMAN_NON_INTERACTIVE=1 \
                GIT_CONFIG_NOSYSTEM=1 \
                GIT_ALLOW_PROTOCOL=file \
                GIT_CONFIG_PARAMETERS="'url.file:///dotman-e2e-no-network/.insteadOf=https://' 'url.file:///dotman-e2e-no-network/.insteadOf=http://'" \
                "$@"
    )
}

# Runs git, or another tool, outside every machine, with a throwaway HOME in the temp root.
e2e_tool() {
    (
        cd "$E2E_ROOT" &&
            exec env -i HOME="$E2E_ROOT/tool/home" PATH="$E2E_PATH" LC_ALL=C GIT_CONFIG_NOSYSTEM=1 \
                GIT_ALLOW_PROTOCOL=file "$@"
    )
}

# Runs git in machine M's repo, in M's environment.
e2e_git() {
    local m=$1
    shift
    e2e_on "$m" git -C "$(e2e_repo "$m")" "$@"
}

# Runs dotman on machine M with stdin closed; sets E2E_RC, E2E_OUT and E2E_ERR, never fails.
e2e_dm() {
    local m=$1
    shift
    E2E_LAST_MACHINE=$m
    E2E_LAST_CMD="dotman $*"
    E2E_RC=0
    # Checked here as well, so a refusal is not lost in the redirected output below.
    e2e_guard "$m"
    e2e_on "$m" dotman "$@" </dev/null >"$E2E_ROOT/last.out" 2>"$E2E_ROOT/last.err" || E2E_RC=$?
    E2E_OUT=$(<"$E2E_ROOT/last.out")
    E2E_ERR=$(<"$E2E_ROOT/last.err")
    {
        printf '\n[%s] $ %s  (exit %s)\n' "$m" "$E2E_LAST_CMD" "$E2E_RC"
        cat "$E2E_ROOT/last.out" "$E2E_ROOT/last.err"
    } >>"$E2E_ROOT/log"
}

# Runs dotman on machine M and asserts exit RC: e2e_dm_rc RC M ARGS...
e2e_dm_rc() {
    local want=$1
    shift
    e2e_dm "$@"
    e2e_rc_is "$want" "$1: dotman ${*:2}"
}

# Creates the first repo on machine M, host HOST, with the shared remote as origin.
e2e_init() {
    e2e_dm_rc 0 "$1" init --no-import --remote "$(e2e_remote_url)" --host "$2" --no-system --no-key
}

# Clones the shared remote onto machine M, host HOST, and applies it.
e2e_join() {
    e2e_dm_rc 0 "$1" init "$(e2e_remote_url)" --host "$2" "${@:3}"
}

# Records a passing assertion; the temp root prints as $ROOT.
e2e_pass() {
    E2E_PASS=$((E2E_PASS + 1))
    printf 'PASS  [%s] %s\n' "$E2E_NAME" "${1//"$E2E_ROOT"/\$ROOT}"
}

# Records a failing assertion, with the last dotman call and its output for context.
e2e_fail() {
    E2E_FAIL=$((E2E_FAIL + 1))
    printf 'FAIL  [%s] %s\n' "$E2E_NAME" "${1//"$E2E_ROOT"/\$ROOT}"
    if [[ -n $E2E_LAST_CMD ]]; then
        printf '      last: [%s] %s (exit %s)\n' "$E2E_LAST_MACHINE" "$E2E_LAST_CMD" "$E2E_RC"
        cat "$E2E_ROOT/last.out" "$E2E_ROOT/last.err" | head -n 40 | sed 's/^/      | /'
    fi
}

# Records an invariant violation, which run.sh also counts on its own.
e2e_violation() {
    E2E_VIOLATIONS=$((E2E_VIOLATIONS + 1))
    e2e_fail "invariant: $1"
}

# Asserts that a command succeeds: e2e_check DESC CMD...
e2e_check() {
    local desc=$1
    shift
    if "$@"; then e2e_pass "$desc"; else e2e_fail "$desc"; fi
}

# Asserts that a command fails: e2e_check_not DESC CMD...
e2e_check_not() {
    local desc=$1
    shift
    if "$@"; then e2e_fail "$desc"; else e2e_pass "$desc"; fi
}

# Asserts that the last dotman call exited with RC.
e2e_rc_is() {
    if [[ $E2E_RC == "$1" ]]; then
        e2e_pass "${2:-$E2E_LAST_CMD} exits $1"
    else
        e2e_fail "${2:-$E2E_LAST_CMD} exits $1 (got $E2E_RC)"
    fi
}

# Asserts that the last dotman call's stderr contains, or lacks, a fixed string.
e2e_err_has() {
    if grep -qF -- "$1" "$E2E_ROOT/last.err"; then e2e_pass "stderr names '$1'"; else e2e_fail "stderr names '$1'"; fi
}
e2e_err_lacks() {
    if grep -qF -- "$1" "$E2E_ROOT/last.err"; then e2e_fail "stderr lacks '$1'"; else e2e_pass "stderr lacks '$1'"; fi
}

# Asserts that no stderr line of the last dotman call that names WHERE also names TEXT.
e2e_err_line_lacks() {
    if grep -F -- "$1" "$E2E_ROOT/last.err" | grep -qF -- "$2"; then
        e2e_fail "no stderr line naming '$1' names '$2'"
    else
        e2e_pass "no stderr line naming '$1' names '$2'"
    fi
}

# Asserts that the last dotman call's stdout contains, or lacks, a fixed string.
e2e_out_has() {
    if grep -qF -- "$1" "$E2E_ROOT/last.out"; then e2e_pass "stdout names '$1'"; else e2e_fail "stdout names '$1'"; fi
}
e2e_out_lacks() {
    if grep -qF -- "$1" "$E2E_ROOT/last.out"; then e2e_fail "stdout lacks '$1'"; else e2e_pass "stdout lacks '$1'"; fi
}

# Records the bytes at PATH as a hand edit, under LABEL, for the no-loss invariant. A later
# edit under the same label replaces the earlier one, which the user overwrote themselves.
e2e_ledger() {
    local label=$1 path=$2 sum
    sum=$(sha256sum <"$path" | cut -d' ' -f1)
    cp -- "$path" "$E2E_ROOT/ledger/$sum"
    awk -F'\t' -v label="$label" '$1 != label' "$E2E_ROOT/ledger/edits" >"$E2E_ROOT/ledger/edits.new"
    printf '%s\t%s\n' "$label" "$sum" >>"$E2E_ROOT/ledger/edits.new"
    mv -- "$E2E_ROOT/ledger/edits.new" "$E2E_ROOT/ledger/edits"
}

# Writes TEXT to REL under machine M's HOME at MODE (default 644), as a tracked hand edit.
e2e_write() {
    local m=$1 rel=$2 text=$3 mode=${4:-644} path
    path="$(e2e_home "$m")/$rel"
    mkdir -p "$(dirname "$path")"
    printf '%s' "$text" >"$path"
    chmod "$mode" "$path"
    e2e_ledger "$m:~/$rel" "$path"
}

# Writes TEXT to REL in machine M's repo worktree, as a tracked hand edit.
e2e_write_repo() {
    local m=$1 rel=$2 text=$3 path
    path="$(e2e_repo "$m")/$rel"
    mkdir -p "$(dirname "$path")"
    printf '%s' "$text" >"$path"
    e2e_ledger "$m:repo/$rel" "$path"
}

# Records that REL must never be wider than octal MODE in machine M's HOME, or in every HOME
# when no M is given.
e2e_mode_cap() {
    printf '%s\t%s\t%s\n' "$1" "$2" "${3:-*}" >>"$E2E_ROOT/ledger/modes"
}

# Drops the caps on REL, once an explicit [modes] entry widens it on purpose.
e2e_mode_uncap() {
    awk -F'\t' -v rel="$1" '$1 != rel' "$E2E_ROOT/ledger/modes" >"$E2E_ROOT/ledger/modes.new"
    mv -- "$E2E_ROOT/ledger/modes.new" "$E2E_ROOT/ledger/modes"
}

# Asserts that REL under machine M's HOME holds exactly TEXT.
e2e_file_is() {
    local path got
    path="$(e2e_home "$1")/$2"
    got=$(
        cat -- "$path" 2>/dev/null
        printf x
    )
    if [[ -f $path && $got == "$3x" ]]; then
        e2e_pass "$1:~/$2 holds the expected text"
    else
        e2e_fail "$1:~/$2 holds the expected text"
    fi
}

# Asserts that REL under machine M's HOME contains, or lacks, a fixed string.
e2e_file_has() {
    local path
    path="$(e2e_home "$1")/$2"
    if [[ -f $path ]] && grep -qF -- "$3" "$path"; then
        e2e_pass "$1:~/$2 contains '$3'"
    else
        e2e_fail "$1:~/$2 contains '$3'"
    fi
}
e2e_file_lacks() {
    local path
    path="$(e2e_home "$1")/$2"
    if [[ -f $path ]] && ! grep -qF -- "$3" "$path"; then
        e2e_pass "$1:~/$2 lacks '$3'"
    else
        e2e_fail "$1:~/$2 lacks '$3'"
    fi
}

# Asserts that REL under machine M's HOME has octal MODE, such as 600.
e2e_mode_is() {
    local got
    got=$(stat -c '%a' -- "$(e2e_home "$1")/$2" 2>/dev/null || printf 'missing')
    if [[ $got == "$3" ]]; then e2e_pass "$1:~/$2 is mode $3"; else e2e_fail "$1:~/$2 is mode $3 (got $got)"; fi
}

# Asserts that REL under machine M's HOME does not exist.
e2e_absent() {
    if [[ -e "$(e2e_home "$1")/$2" || -L "$(e2e_home "$1")/$2" ]]; then
        e2e_fail "$1:~/$2 is absent"
    else
        e2e_pass "$1:~/$2 is absent"
    fi
}

# Asserts that the remote's main branch tracks PATH, or does not.
e2e_remote_tracks() {
    if e2e_tool git --git-dir="$(e2e_remote)" cat-file -e "main:$1" 2>/dev/null; then
        e2e_pass "the remote tracks $1"
    else
        e2e_fail "the remote tracks $1"
    fi
}
e2e_remote_lacks() {
    if e2e_tool git --git-dir="$(e2e_remote)" cat-file -e "main:$1" 2>/dev/null; then
        e2e_fail "the remote lacks $1"
    else
        e2e_pass "the remote lacks $1"
    fi
}

# Asserts that no object reachable on the remote contains the fixed string TEXT.
e2e_remote_never_holds() {
    local gd objects hits
    gd=$(e2e_remote)
    objects=$(e2e_tool git --git-dir="$gd" rev-list --all --objects | cut -d' ' -f1)
    hits=$(printf '%s\n' "$objects" | e2e_tool git --git-dir="$gd" cat-file --batch | grep -acF -- "$1") || true
    if [[ ${hits:-0} == 0 ]]; then
        e2e_pass "no object on the remote holds '$1'"
    else
        e2e_fail "no object on the remote holds '$1' ($hits lines do)"
    fi
}

# Prints the remote's copy of PATH on main.
e2e_remote_show() {
    e2e_tool git --git-dir="$(e2e_remote)" show "main:$1"
}

# Prints a GitHub-token-shaped string built at run time, so no literal token is in the source.
e2e_token() {
    local body
    body=$(head -c 64 /dev/urandom | sha256sum | cut -c1-36)
    printf 'gh%s_%s' p "$body"
}

# Lets the no-loss invariant decrypt with the age identity at PATH.
e2e_identity_known() {
    printf '%s\n' "$1" >>"$E2E_ROOT/ledger/identities"
}

# Creates an age identity for machine M with age-keygen inside the temp root, registers it with
# the no-loss invariant, and prints its path.
e2e_age_identity() {
    local path
    path="$(e2e_mroot "$1")/age-identity.txt"
    e2e_on "$1" age-keygen -o "$path" 2>/dev/null
    e2e_identity_known "$path"
    printf '%s' "$path"
}

# Prints CMD when it is a runnable command: it starts with a command name dotman prints and has
# no <placeholder>. Anything else goes to fd 3 when it is open.
e2e_recipe_emit() {
    case $1 in
        *'<'*'>'*) ;;
        "dotman "* | "git "* | "rm "* | "mv "* | "cp "*)
            printf '%s\n' "$1"
            return 0
            ;;
    esac
    { printf '%s\n' "$1" >&3; } 2>/dev/null || true
}

# Prints the commands the last dotman call printed, stdout first, then stderr, in order: each
# numbered step that is a command, each indented command line under a step, and each backticked
# command.
e2e_recipe() {
    local line cmd
    while IFS= read -r line; do
        if [[ $line =~ ^[[:space:]]*[0-9]+\.[[:space:]]+(.*)$ ]]; then
            line=${BASH_REMATCH[1]}
            if [[ $line != *'`'* ]]; then
                e2e_recipe_emit "$line"
                continue
            fi
        elif [[ $line =~ ^[[:space:]]{2,}((dotman|git|rm|mv|cp)[[:space:]].*)$ && $line != *'`'* ]]; then
            e2e_recipe_emit "${BASH_REMATCH[1]}"
            continue
        fi
        while [[ $line =~ \`([^\`]+)\` ]]; do
            cmd=${BASH_REMATCH[1]}
            line=${line#*"\`$cmd\`"}
            e2e_recipe_emit "$cmd" 3>/dev/null
        done
    done < <(cat "$E2E_ROOT/last.out" "$E2E_ROOT/last.err")
}

# Prints the first command the last dotman call printed that matches the grep -E PATTERN.
e2e_printed() {
    e2e_recipe | { grep -E -- "$1" || true; } | sed -n 1p
}

# Runs CMD exactly as printed, with bash --noprofile --norc in machine M's environment and
# stdin closed; records a failure unless it exits WANT (default 0), and sets E2E_PRINTED_RC.
e2e_run_printed() {
    local m=$1 cmd=$2 want=${3:-0} rc=0
    if [[ -z $cmd ]]; then
        e2e_fail "$m: the expected command is printed"
        E2E_PRINTED_RC=1
        return 0
    fi
    e2e_guard "$m"
    e2e_on "$m" bash --noprofile --norc -c "$cmd" </dev/null >"$E2E_ROOT/recipe.out" 2>&1 || rc=$?
    {
        printf '\n[%s] printed $ %s  (exit %s)\n' "$m" "$cmd" "$rc"
        cat "$E2E_ROOT/recipe.out"
    } >>"$E2E_ROOT/log"
    if [[ $rc == "$want" ]]; then
        e2e_pass "$m: printed \`$cmd\` exits $want"
    else
        e2e_fail "$m: printed \`$cmd\` exits $want (got $rc)"
        head -n 20 "$E2E_ROOT/recipe.out" | sed 's/^/      | /'
    fi
    E2E_PRINTED_RC=$rc
}

# Runs, in order, the printed commands of the last dotman call that match the grep -E PATTERN,
# and asserts there are COUNT of them: e2e_run_recipe M PATTERN COUNT. Stops at the first
# command that fails.
e2e_run_recipe() {
    local m=$1 pattern=$2 count=$3 cmds=() cmd
    mapfile -t cmds < <(e2e_recipe | grep -E -- "$pattern" || true)
    if [[ ${#cmds[@]} != "$count" ]]; then
        e2e_fail "$m: $count printed commands match /$pattern/ (got ${#cmds[@]})"
        return 0
    fi
    e2e_pass "$m: $count printed commands match /$pattern/"
    for cmd in "${cmds[@]}"; do
        e2e_run_printed "$m" "$cmd"
        [[ $E2E_PRINTED_RC == 0 ]] || break
    done
}

# Prints every entry under DIR with its mode, type and link target, then every file's sha256.
e2e_manifest_dir() {
    [[ -e $1 ]] || return 0
    (
        cd "$1" || exit 1
        find . -mindepth 1 -printf '%p\t%m\t%y\t%l\n' | LC_ALL=C sort
        find . -type f -exec sha256sum -- {} + | LC_ALL=C sort -k2
    )
}

# Prints the checksum manifest of every machine's HOME and XDG dirs, and of the remote.
e2e_snapshot() {
    local m
    for m in "${E2E_MACHINES[@]}"; do
        printf '## %s\n' "$m"
        e2e_manifest_dir "$(e2e_mroot "$m")/home"
        e2e_manifest_dir "$(e2e_mroot "$m")/xdg"
    done
    printf '## remote\n'
    e2e_manifest_dir "$(e2e_remote)"
}

# Runs dotman ARGS --dry-run on machine M and records a violation if it changed anything
# anywhere.
e2e_dry_run() {
    local m=$1 before after
    shift
    before=$(e2e_snapshot)
    e2e_dm "$m" "$@" --dry-run
    after=$(e2e_snapshot)
    if [[ $before == "$after" ]]; then
        e2e_pass "$m: dotman $* --dry-run changes nothing (exit $E2E_RC)"
    else
        e2e_violation "$m: dotman $* --dry-run changes nothing"
        { diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") || true; } | head -n 20 | sed 's/^/      | /'
    fi
}

# Prints the git dirs of the remote and of every machine's repo that exist.
e2e_git_dirs() {
    local m
    [[ -d $(e2e_remote) ]] && printf '%s\n' "$(e2e_remote)"
    for m in "${E2E_MACHINES[@]}"; do
        [[ -d "$(e2e_repo "$m")/.git" ]] && printf '%s\n' "$(e2e_repo "$m")/.git"
    done
    return 0
}

# Prints the sha256 of every age-encrypted object in GD that a registered identity decrypts.
e2e_decrypted_sums() {
    local gd=$1 id oid
    [[ -s $E2E_ROOT/ledger/identities ]] || return 0
    while read -r oid; do
        e2e_tool git --git-dir="$gd" cat-file blob "$oid" >"$E2E_ROOT/blob" 2>/dev/null || continue
        # dotman stores armored ciphertext; the binary header is accepted as well.
        case $(head -c 34 "$E2E_ROOT/blob") in
            "-----BEGIN AGE ENCRYPTED FILE-----" | age-encryption.org/v1*) ;;
            *) continue ;;
        esac
        while IFS= read -r id; do
            if e2e_tool age -d -i "$id" -o "$E2E_ROOT/plain" "$E2E_ROOT/blob" 2>/dev/null; then
                sha256sum <"$E2E_ROOT/plain" | cut -d' ' -f1
                break
            fi
        done <"$E2E_ROOT/ledger/identities"
    done < <(e2e_tool git --git-dir="$gd" cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype)' |
        awk '$2 == "blob" { print $1 }')
    rm -f -- "$E2E_ROOT/blob" "$E2E_ROOT/plain"
}

# Invariant: every hand edit's bytes are still somewhere: a file under some machine's HOME or
# XDG dirs (repo worktree, backups, saved repo edits), or an object in a repo or the remote,
# encrypted or not.
e2e_inv_no_loss() {
    local label sum path gd blob lost=0 decrypted=0
    local -A found=()
    while IFS= read -r -d '' path; do
        found[$(sha256sum <"$path" | cut -d' ' -f1)]=1
    done < <(find "$E2E_ROOT/m" -type f -print0 2>/dev/null)
    while IFS=$'\t' read -r label sum; do
        [[ -n ${found[$sum]:-} ]] && continue
        blob=$(e2e_tool git hash-object --stdin <"$E2E_ROOT/ledger/$sum")
        while IFS= read -r gd; do
            if e2e_tool git --git-dir="$gd" cat-file -e "$blob" 2>/dev/null; then
                found[$sum]=1
                break
            fi
        done < <(e2e_git_dirs)
        if [[ -z ${found[$sum]:-} && $decrypted == 0 ]]; then
            decrypted=1
            while IFS= read -r gd; do
                while IFS= read -r blob; do found[$blob]=1; done < <(e2e_decrypted_sums "$gd")
            done < <(e2e_git_dirs)
        fi
        [[ -n ${found[$sum]:-} ]] && continue
        lost=1
        e2e_violation "the edit $label is lost (sha256 $sum)"
    done <"$E2E_ROOT/ledger/edits"
    [[ $lost == 1 ]] || e2e_pass "invariant: no hand edit is lost"
}

# Invariant: no object reachable on the remote holds a GitHub token or an age identity.
e2e_inv_no_secrets() {
    local gd=$E2E_ROOT/remote.git objects hits
    [[ -d $gd ]] || return 0
    objects=$(e2e_tool git --git-dir="$gd" rev-list --all --objects | cut -d' ' -f1)
    hits=$(printf '%s\n' "$objects" | e2e_tool git --git-dir="$gd" cat-file --batch |
        grep -acE 'gh[pousr]_[A-Za-z0-9]{36}|AGE-SECRET-KEY-1') || true
    if [[ ${hits:-0} == 0 ]]; then
        e2e_pass "invariant: nothing secret-shaped reaches the remote"
    else
        e2e_violation "nothing secret-shaped reaches the remote ($hits lines hold one)"
    fi
}

# Invariant: no commit in the remote or in any machine's repo holds a .dotman-tmp-* path.
e2e_inv_no_temp_committed() {
    local gd paths bad=0
    while IFS= read -r gd; do
        paths=$(e2e_tool git --git-dir="$gd" rev-list --all --objects 2>/dev/null) || true
        if [[ $paths == *.dotman-tmp-* ]]; then
            bad=1
            e2e_violation "a .dotman-tmp-* file is committed in $gd"
        fi
    done < <(e2e_git_dirs)
    [[ $bad == 1 ]] || e2e_pass "invariant: no .dotman-tmp-* file is committed"
}

# Invariant: no capped file is wider than its cap in any HOME, and no HOME file carries a
# setuid, setgid or sticky bit.
e2e_inv_modes() {
    local m rel cap only path got bad=0
    while IFS=$'\t' read -r rel cap only; do
        for m in "${E2E_MACHINES[@]}"; do
            [[ $only == '*' || $only == "$m" ]] || continue
            path="$(e2e_home "$m")/$rel"
            [[ -f $path && ! -L $path ]] || continue
            got=$(stat -c '%a' -- "$path")
            if (((8#$got & ~8#$cap) != 0)); then
                bad=1
                e2e_violation "$m:~/$rel is $got, wider than $cap"
            fi
        done
    done <"$E2E_ROOT/ledger/modes"
    for m in "${E2E_MACHINES[@]}"; do
        if [[ -n $(find "$(e2e_home "$m")" -perm /7000 -print -quit) ]]; then
            bad=1
            e2e_violation "$m: a HOME file has a setuid, setgid or sticky bit"
        fi
    done
    [[ $bad == 1 ]] || e2e_pass "invariant: no mode is widened"
}

# Runs every invariant, prints the scenario's counts and exits non-zero on any failure.
e2e_finish() {
    E2E_LAST_CMD=
    e2e_inv_no_loss
    e2e_inv_no_secrets
    e2e_inv_no_temp_committed
    e2e_inv_modes
    E2E_FINISHED=1
    e2e_record
    printf -- '--- %s: %s passed, %s failed, %s invariant violations\n' \
        "$E2E_NAME" "$E2E_PASS" "$E2E_FAIL" "$E2E_VIOLATIONS"
    [[ $E2E_FAIL == 0 ]]
    exit $?
}
