
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'dotman' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'dotman'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'dotman' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('init', 'init', [CompletionResultType]::ParameterValue, 'Create the dotman repo or set its remote, or clone a repo and apply it')
            [CompletionResult]::new('add', 'add', [CompletionResultType]::ParameterValue, 'Start managing files or directories')
            [CompletionResult]::new('forget', 'forget', [CompletionResultType]::ParameterValue, 'Stop managing files or directories, keeping them in $HOME')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List the managed files')
            [CompletionResult]::new('status', 'status', [CompletionResultType]::ParameterValue, 'Compare $HOME, the repo and the last applied state')
            [CompletionResult]::new('apply', 'apply', [CompletionResultType]::ParameterValue, 'Write the repo''s files into $HOME')
            [CompletionResult]::new('diff', 'diff', [CompletionResultType]::ParameterValue, 'Show how $HOME differs from the repo, as a diff from repo/ to home/')
            [CompletionResult]::new('edit', 'edit', [CompletionResultType]::ParameterValue, 'Open a managed file''s source in $VISUAL or $EDITOR, then apply it')
            [CompletionResult]::new('sync', 'sync', [CompletionResultType]::ParameterValue, 'Capture local changes, commit, pull, apply what arrived and push')
            [CompletionResult]::new('log', 'log', [CompletionResultType]::ParameterValue, 'Show the repo''s history')
            [CompletionResult]::new('restore', 'restore', [CompletionResultType]::ParameterValue, 'Write an earlier version of a file into $HOME, keeping the current one as a backup')
            [CompletionResult]::new('import', 'import', [CompletionResultType]::ParameterValue, 'Convert another setup into the repo, or add what the repo lacks from one')
            [CompletionResult]::new('git', 'git', [CompletionResultType]::ParameterValue, 'Run git in the repo')
            [CompletionResult]::new('doctor', 'doctor', [CompletionResultType]::ParameterValue, 'Check the environment and the repo for problems')
            [CompletionResult]::new('key', 'key', [CompletionResultType]::ParameterValue, 'Create or import the age identity that decrypts encrypted files')
            [CompletionResult]::new('completion', 'completion', [CompletionResultType]::ParameterValue, 'Generate shell completion scripts')
            [CompletionResult]::new('system-write', 'system-write', [CompletionResultType]::ParameterValue, 'Write the system files a plan names; `apply --system` starts it through sudo, doas or run0')
            break
        }
        'dotman;init' {
            [CompletionResult]::new('--remote', '--remote', [CompletionResultType]::ParameterName, 'Set or replace the `origin` remote')
            [CompletionResult]::new('--git-name', '--git-name', [CompletionResultType]::ParameterName, 'Git `user.name` to set when git has no identity')
            [CompletionResult]::new('--git-email', '--git-email', [CompletionResultType]::ParameterName, 'Git `user.email` to set when git has no identity')
            [CompletionResult]::new('--create-github', '--create-github', [CompletionResultType]::ParameterName, 'Create the private GitHub repo `OWNER/NAME` with `gh` and make it `origin`')
            [CompletionResult]::new('--host', '--host', [CompletionResultType]::ParameterName, 'Host name of this machine, written to `[identity]` in the machine config')
            [CompletionResult]::new('--profile', '--profile', [CompletionResultType]::ParameterName, 'A profile this machine opts into, written to the machine config; repeatable')
            [CompletionResult]::new('--system-root', '--system-root', [CompletionResultType]::ParameterName, 'A directory outside $HOME whose files this machine manages, added to `[system] roots`; repeatable')
            [CompletionResult]::new('--age-key-file', '--age-key-file', [CompletionResultType]::ParameterName, 'Import the age identity in FILE before applying, so encrypted files decrypt')
            [CompletionResult]::new('--no-import', '--no-import', [CompletionResultType]::ParameterName, 'Create a fresh repo without looking for an existing setup to import')
            [CompletionResult]::new('--keep-local', '--keep-local', [CompletionResultType]::ParameterName, 'Keep the local version of files that differ from the repo')
            [CompletionResult]::new('--take-repo', '--take-repo', [CompletionResultType]::ParameterName, 'Take the repo version of files that differ, after backing them up')
            [CompletionResult]::new('--approve-hooks', '--approve-hooks', [CompletionResultType]::ParameterName, 'Approve the `[hooks]` table of the cloned repo on this machine and run its hooks')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be created and change nothing')
            [CompletionResult]::new('--no-bootstrap', '--no-bootstrap', [CompletionResultType]::ParameterName, 'Do not write install.sh, the script that sets up `dotman` in a container')
            [CompletionResult]::new('--refresh-bootstrap', '--refresh-bootstrap', [CompletionResultType]::ParameterName, 'Rewrite install.sh with this version''s script, unless it was edited')
            [CompletionResult]::new('--git-global', '--git-global', [CompletionResultType]::ParameterName, 'Store --git-name and --git-email in the global git config instead of the repo''s')
            [CompletionResult]::new('--no-remote', '--no-remote', [CompletionResultType]::ParameterName, 'Do not ask for a remote')
            [CompletionResult]::new('--no-system', '--no-system', [CompletionResultType]::ParameterName, 'Do not ask about system files')
            [CompletionResult]::new('--create-key', '--create-key', [CompletionResultType]::ParameterName, 'Create an age identity for encrypted files')
            [CompletionResult]::new('--no-key', '--no-key', [CompletionResultType]::ParameterName, 'Do not ask about an age identity')
            [CompletionResult]::new('--force', '--force', [CompletionResultType]::ParameterName, 'With --age-key-file: replace a different existing identity, after backing it up')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;add' {
            [CompletionResult]::new('--for', '--for', [CompletionResultType]::ParameterName, 'Add the files as variants that apply only where these `+`-joined conditions hold, such as `host=laptop+os!=macos`')
            [CompletionResult]::new('--encrypt', '--encrypt', [CompletionResultType]::ParameterName, 'Store the files encrypted to the `[age] recipients` in .dotman.toml')
            [CompletionResult]::new('--allow-plain', '--allow-plain', [CompletionResultType]::ParameterName, 'Store in plain text what the guard holds back: files whose names match `[guard] secret_names`, and credentials in files named here, not ones found in a directory; never an age secret key')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'Do not ask before adding directories')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be added and change nothing')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Add regular files outside $HOME that you can read, such as /etc/keyd/default.conf')
            [CompletionResult]::new('--package-owned', '--package-owned', [CompletionResultType]::ParameterName, 'With --system: the file belongs to a package; manage it anyway')
            [CompletionResult]::new('--add-root', '--add-root', [CompletionResultType]::ParameterName, 'With --system: add the top-level directory to `[system] roots` without asking')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;forget' {
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be forgotten and change nothing')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;list' {
            [CompletionResult]::new('--porcelain', '--porcelain', [CompletionResultType]::ParameterName, 'Print `<kind>\t<key>\0` records')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'dotman;status' {
            [CompletionResult]::new('--porcelain', '--porcelain', [CompletionResultType]::ParameterName, 'Print `<status>\t<key>\0` records for every managed path')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'dotman;apply' {
            [CompletionResult]::new('--keep-local', '--keep-local', [CompletionResultType]::ParameterName, 'Keep the local version of conflicting and existing files')
            [CompletionResult]::new('--take-repo', '--take-repo', [CompletionResultType]::ParameterName, 'Take the repo version of conflicting and existing files, after backing them up')
            [CompletionResult]::new('--forget-deleted', '--forget-deleted', [CompletionResultType]::ParameterName, 'Stop managing files deleted locally; they stay deleted')
            [CompletionResult]::new('--restore-deleted', '--restore-deleted', [CompletionResultType]::ParameterName, 'Write files deleted locally back from the repo')
            [CompletionResult]::new('--accept-identity', '--accept-identity', [CompletionResultType]::ParameterName, 'Accept a changed machine identity and use the variants it selects')
            [CompletionResult]::new('--approve-hooks', '--approve-hooks', [CompletionResultType]::ParameterName, 'Approve the current `[hooks]` table on this machine and run its hooks')
            [CompletionResult]::new('--in-place', '--in-place', [CompletionResultType]::ParameterName, 'Rewrite files in place when a rename cannot replace them, as on a bind mount')
            [CompletionResult]::new('--no-hooks', '--no-hooks', [CompletionResultType]::ParameterName, 'Run no hooks')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be written and change nothing')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'Also write the system files outside $HOME, through one sudo, doas or run0 call')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'With --system: write the system files without asking')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;diff' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'dotman;edit' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;sync' {
            [CompletionResult]::new('-m', '-m', [CompletionResultType]::ParameterName, 'Commit message, replacing the generated `sync(<host>): ...` line')
            [CompletionResult]::new('--message', '--message', [CompletionResultType]::ParameterName, 'Commit message, replacing the generated `sync(<host>): ...` line')
            [CompletionResult]::new('--keep-local', '--keep-local', [CompletionResultType]::ParameterName, 'Keep the local version of conflicting and existing files')
            [CompletionResult]::new('--take-repo', '--take-repo', [CompletionResultType]::ParameterName, 'Take the repo version of conflicting and existing files, after backing them up')
            [CompletionResult]::new('--forget-deleted', '--forget-deleted', [CompletionResultType]::ParameterName, 'Stop managing files deleted locally; they stay deleted')
            [CompletionResult]::new('--restore-deleted', '--restore-deleted', [CompletionResultType]::ParameterName, 'Write files deleted locally back from the repo')
            [CompletionResult]::new('--accept-identity', '--accept-identity', [CompletionResultType]::ParameterName, 'Accept a changed machine identity and use the variants it selects')
            [CompletionResult]::new('--approve-hooks', '--approve-hooks', [CompletionResultType]::ParameterName, 'Approve the current `[hooks]` table on this machine and run its hooks')
            [CompletionResult]::new('--no-hooks', '--no-hooks', [CompletionResultType]::ParameterName, 'Run no hooks')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be captured, committed, pulled, applied and pushed; change nothing')
            [CompletionResult]::new('--system', '--system', [CompletionResultType]::ParameterName, 'After the pull, also write the system files outside $HOME, through one sudo, doas or run0 call')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'With --system: write the system files without asking')
            [CompletionResult]::new('--allow-mass-delete', '--allow-mass-delete', [CompletionResultType]::ParameterName, 'Commit even when the repo changes delete at least half of the tracked files (10 or more)')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'dotman;log' {
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Show at most this many commits')
            [CompletionResult]::new('--max-count', '--max-count', [CompletionResultType]::ParameterName, 'Show at most this many commits')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;restore' {
            [CompletionResult]::new('--from', '--from', [CompletionResultType]::ParameterName, 'Revision or date to restore from; `HEAD` when omitted')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show what would be written and change nothing')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;import' {
            [CompletionResult]::new('--overlay', '--overlay', [CompletionResultType]::ParameterName, 'Add what SOURCE has and the repo lacks as one commit: modes, and with `--include-missing` files')
            [CompletionResult]::new('--include-missing', '--include-missing', [CompletionResultType]::ParameterName, 'With `--overlay`, add the files the repo lacks without asking')
            [CompletionResult]::new('--render-templates', '--render-templates', [CompletionResultType]::ParameterName, 'For a chezmoi source, run its templates and import this host''s rendering')
            [CompletionResult]::new('--swap', '--swap', [CompletionResultType]::ParameterName, 'Replace the symlinks of an imported farm with real files holding the same bytes')
            [CompletionResult]::new('--keep-history', '--keep-history', [CompletionResultType]::ParameterName, 'For a git source whose history holds a credential or an age secret key, import every commit instead of one commit of the current tree')
            [CompletionResult]::new('--yes', '--yes', [CompletionResultType]::ParameterName, 'Do not ask before writing')
            [CompletionResult]::new('--no-bootstrap', '--no-bootstrap', [CompletionResultType]::ParameterName, 'Do not write install.sh, the script that sets up `dotman` in a container')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Show the report and change nothing')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;git' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            break
        }
        'dotman;doctor' {
            [CompletionResult]::new('--online', '--online', [CompletionResultType]::ParameterName, 'Also ask the remote with `git ls-remote`')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;key' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('init', 'init', [CompletionResultType]::ParameterValue, 'Generate an age identity and add its recipient to .dotman.toml')
            [CompletionResult]::new('import', 'import', [CompletionResultType]::ParameterValue, 'Copy an existing age identity into place and check that its recipient is listed')
            break
        }
        'dotman;key;init' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;key;import' {
            [CompletionResult]::new('--add-recipient', '--add-recipient', [CompletionResultType]::ParameterName, 'Add the identity''s recipient to `[age] recipients` in .dotman.toml if it is missing')
            [CompletionResult]::new('--force', '--force', [CompletionResultType]::ParameterName, 'Replace an existing, different identity, after backing it up')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;completion' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'dotman;system-write' {
            [CompletionResult]::new('--plan', '--plan', [CompletionResultType]::ParameterName, 'The plan file that `apply --system` wrote')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress informational messages')
            [CompletionResult]::new('--no-pager', '--no-pager', [CompletionResultType]::ParameterName, 'Disable pager output')
            [CompletionResult]::new('--allow-root', '--allow-root', [CompletionResultType]::ParameterName, 'Allow running as root')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}
