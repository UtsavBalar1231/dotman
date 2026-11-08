_dotman() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="dotman"
                ;;
            dotman,add)
                cmd="dotman__add"
                ;;
            dotman,apply)
                cmd="dotman__apply"
                ;;
            dotman,completion)
                cmd="dotman__completion"
                ;;
            dotman,diff)
                cmd="dotman__diff"
                ;;
            dotman,doctor)
                cmd="dotman__doctor"
                ;;
            dotman,edit)
                cmd="dotman__edit"
                ;;
            dotman,forget)
                cmd="dotman__forget"
                ;;
            dotman,git)
                cmd="dotman__git"
                ;;
            dotman,import)
                cmd="dotman__import"
                ;;
            dotman,init)
                cmd="dotman__init"
                ;;
            dotman,key)
                cmd="dotman__key"
                ;;
            dotman,list)
                cmd="dotman__list"
                ;;
            dotman,log)
                cmd="dotman__log"
                ;;
            dotman,restore)
                cmd="dotman__restore"
                ;;
            dotman,status)
                cmd="dotman__status"
                ;;
            dotman,sync)
                cmd="dotman__sync"
                ;;
            dotman,system-write)
                cmd="dotman__system__write"
                ;;
            dotman__key,import)
                cmd="dotman__key__import"
                ;;
            dotman__key,init)
                cmd="dotman__key__init"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        dotman)
            opts="-v -q -h -V --verbose --quiet --no-pager --allow-root --help --version init add forget list status apply diff edit sync log restore import git doctor key completion system-write"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__add)
            opts="-v -q -h --for --encrypt --allow-plain --yes --dry-run --system --package-owned --add-root --verbose --quiet --no-pager --allow-root --help <PATHS>..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --for)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__apply)
            opts="-v -q -h --keep-local --take-repo --forget-deleted --restore-deleted --accept-identity --approve-hooks --in-place --no-hooks --dry-run --system --yes --verbose --quiet --no-pager --allow-root --help [PATHS]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__completion)
            opts="-v -q -h --verbose --quiet --no-pager --allow-root --help bash elvish fish powershell zsh"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__diff)
            opts="-v -q -h --verbose --quiet --no-pager --allow-root --help [PATHS]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__doctor)
            opts="-v -q -h --online --verbose --quiet --no-pager --allow-root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__edit)
            opts="-v -q -h --verbose --quiet --no-pager --allow-root --help <PATH>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__forget)
            opts="-v -q -h --dry-run --verbose --quiet --no-pager --allow-root --help <PATHS>..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__git)
            opts="-v -q --verbose --quiet --no-pager --allow-root [ARGS]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__import)
            opts="-v -q -h --overlay --include-missing --render-templates --swap --yes --no-bootstrap --dry-run --verbose --quiet --no-pager --allow-root --help [SOURCE]"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --overlay)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__init)
            opts="-v -q -h --remote --no-import --keep-local --take-repo --approve-hooks --dry-run --no-bootstrap --refresh-bootstrap --git-name --git-email --git-global --create-github --no-remote --host --profile --system-root --no-system --create-key --no-key --age-key-file --force --verbose --quiet --no-pager --allow-root --help [URL_OR_PATH]"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --remote)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --git-name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --git-email)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --create-github)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --host)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --system-root)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --age-key-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__key)
            opts="-v -q -h --verbose --quiet --no-pager --allow-root --help init import"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__key__import)
            opts="-v -q -h --add-recipient --force --verbose --quiet --no-pager --allow-root --help [FILE]"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__key__init)
            opts="-v -q -h --verbose --quiet --no-pager --allow-root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__list)
            opts="-v -q -h --porcelain --verbose --quiet --no-pager --allow-root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__log)
            opts="-n -v -q -h --max-count --verbose --quiet --no-pager --allow-root --help [PATHS]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --max-count)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -n)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__restore)
            opts="-v -q -h --from --dry-run --verbose --quiet --no-pager --allow-root --help <PATH>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --from)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__status)
            opts="-v -q -h --porcelain --verbose --quiet --no-pager --allow-root --help [PATHS]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__sync)
            opts="-m -v -q -h --message --keep-local --take-repo --forget-deleted --restore-deleted --accept-identity --approve-hooks --no-hooks --dry-run --system --yes --verbose --quiet --no-pager --allow-root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --message)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -m)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        dotman__system__write)
            opts="-v -q -h --plan --verbose --quiet --no-pager --allow-root --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --plan)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _dotman -o nosort -o bashdefault -o default dotman
else
    complete -F _dotman -o bashdefault -o default dotman
fi
