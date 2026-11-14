# Secrets

Encrypted files, the age identity, and the guards that keep credentials out of the repo. Back to the [README](../README.md).

https://github.com/user-attachments/assets/8bde50d1-c0ee-4340-8b72-7202184c55ae

`dotman add --encrypt <file>` stores the file as age ciphertext, encrypted to every recipient in `.dotman.toml`. `apply` decrypts it to `$HOME` at mode 0600 unless you set `[modes]` by hand. Plaintext is never written into the repo.

Without a key, encrypted files are reported as `locked` and skipped.

```bash
dotman key init                  # new identity at ~/.config/dotman/age-identity.txt
dotman key import key.txt        # use an existing identity; --add-recipient adds its recipient
```

On a machine where you cannot place a file, set `DOTMAN_AGE_KEY` to the key text, or `DOTMAN_AGE_KEY_FILE` to a path. The key goes only to age's stdin and is stripped from every other child process. It is refused in `[template] env`. `DOTMAN_AGE_KEY` works with any age or rage that reads an identity from `-i -`: dotman checks that by decrypting a built-in test file, so it needs no `age-keygen`. Without an env var, the identity file is `$XDG_CONFIG_HOME/dotman/age-identity.txt`.

Back the identity up now: keep it in a password manager and one offline copy. Without an identity, the encrypted data cannot be read, and no one can recover it. Add a second recipient (`dotman key init` on another machine, or `dotman key import --add-recipient`); `dotman doctor` warns while there is only one. When `[age] recipients` changes, here or upstream, the next `sync` on a machine whose key decrypts every encrypted file re-encrypts them all to the new set, without asking. `dotman key import` fetches origin first, for at most ten seconds, and when origin already lists the identity's recipient it adds nothing, because the next `sync` brings the list here.

## The guards

Two guards keep secrets out of the repo, whatever the remote.

- **The content guard** looks for 20 kinds of credential that start with a fixed provider prefix: GitHub tokens of both kinds, GitLab, Slack tokens and webhooks, AWS access key IDs, Stripe live keys, OpenAI, Anthropic, npm, PyPI, DigitalOcean, SendGrid, Hugging Face, PEM private keys, OpenRouter, Groq, xAI, Perplexity and Docker. It also looks for an age identity: `AGE-SECRET-KEY-1` and 58 more characters, so the bare prefix in a script is not held. A finding names the rule and the line, never the value. It also scans each path's name and each symlink's target, which no flag and no `[guard] allow` entry lets through, since neither can be encrypted. A file with a NUL byte in its first 8000 bytes counts as binary and is not scanned.
- **The name guard** holds new files whose names match its patterns: `.netrc`, `.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `*.kdbx`, `*.gpg`, `credentials*`, `*token*`, `*secret*`, `gh/hosts.yml`, `*.ppk` and `rclone.conf`, at any depth, plus those in `[guard] secret_names` in `.dotman.toml`. The file can add patterns but not remove these, so an edit pushed from another machine cannot let a `*token*` file through.

The content guard runs on every plain capture: `add`, new files under a managed directory, `sync` of a local edit, `import`, and the commit `sync` makes, which also covers `dotman edit` and edits made by hand in the repo.

- `add <file>` refuses a file the content guard holds. `--encrypt` stores it encrypted. `--allow-plain` stores it as it is and records the path and the rule under `[guard] allow` in this machine's `config.toml`, so later captures of that file are not held for that rule. An age identity is never stored in plain text, whatever the flags.
- `add <dir>` adds the rest and holds back each file a guard holds, listing it with the command that stores it, and exits 3. For a file that holds a credential, or a secret-named binary file that the content guard does not scan, the command listed is `add --encrypt`. `add --allow-plain <dir>` releases files held for their names only; a file that holds a credential has to be named.
- A file you name whose name matches `secret_names` is added with a warning. When the remote is known to be public, `add` refuses it unless you pass `--encrypt` or `--allow-plain`. The remote counts as public when an anonymous `git ls-remote` of its https form succeeds. `sync` asks again once a day, and a check that could not tell is asked again after an hour.
- `sync` holds new files and local edits that a guard holds. It lists them, commits the rest, and exits 3.
- `import` leaves out the files the content guard holds and exits 3.

`[guard] allow` is per machine. Entries found in `.dotman.toml` are ignored, with a warning, so an entry another machine pushed never lets a credential out of this one.

The guards miss credentials without a known prefix, such as a password or a generic API key in a config file, and anything in a binary file. Name secret files so that `secret_names` matches them.
