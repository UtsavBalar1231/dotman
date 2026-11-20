use super::{R, TestEnv, bound_input_with, records};
use std::fs;
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Cache(std::path::PathBuf);

struct Container {
    name: String,
    home: std::path::PathBuf,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.home)
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "rm",
                "-f",
                &self.name,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["credential-cache", "--socket"])
            .arg(&self.0)
            .arg("exit")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn run(cmd: &mut Command) -> R {
    let output = cmd.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn tls_certificate(root: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let cert = root.join("tls-cert.pem");
    let config = root.join("tls.cnf");
    fs::write(
        &config,
        "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost\n",
    )?;
    run(Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-config"])
        .arg(&config)
        .args(["-days", "1", "-keyout"])
        .arg(root.join("tls-key.pem"))
        .arg("-out")
        .arg(&cert))?;
    Ok(cert)
}

fn assert_cache_retained(root: &Path, socket: &Path, host: &str, path: &str) -> R {
    let mut get = Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .args(["credential-cache", "--socket"])
        .arg(socket)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    write!(
        get.stdin.take().ok_or("cache stdin")?,
        "protocol=https\nhost={host}\npath={path}\n\n"
    )?;
    let output = get.wait_with_output()?;
    assert!(output.status.success());
    assert!(
        output
            .stdout
            .windows(concat!("fixture", "-secret").len())
            .any(|value| value == concat!("fixture", "-secret").as_bytes())
    );
    Ok(())
}

fn port() -> Result<u16, Box<dyn std::error::Error>> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn wait_port(port: u16) -> R {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err("fixture server did not start".into())
}

fn setup() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"one\ntwo\nthree\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env
}

fn exercise(
    env: &TestEnv,
    url: &str,
    vars: &[(&str, &str)],
    diverged: bool,
    untrusted: bool,
    directory_conflict: bool,
) -> R {
    let repo = env.repo();
    let repo = repo.to_str().ok_or("repo")?;
    env.git(&["-C", repo, "remote", "set-url", "--push", "origin", url]);
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
    let review = bound_input_with(env, &[("GIT_CONFIG_PARAMETERS", "")])?;
    if diverged {
        let other = env.root().join("other-worktree");
        let other = other.to_str().ok_or("other")?;
        env.git(&["clone", &env.remote_url(), other]);
        env.git(&["-C", other, "checkout", "main"]);
        let path = Path::new(other).join(".a");
        if directory_conflict {
            fs::remove_file(&path)?;
            fs::create_dir(&path)?;
            fs::write(path.join("child"), b"remote\n")?;
        } else {
            fs::write(path, b"one\nREMOTE\nthree\n")?;
        }
        env.git(&["-C", other, "add", "-A"]);
        env.git(&["-C", other, "commit", "-m", "remote change"]);
        env.git(&["-C", other, "push", "origin", "HEAD:refs/heads/main"]);
    }
    let remote = env.remote_path();
    let remote = remote.to_str().ok_or("remote")?;
    env.git(&[
        "--git-dir",
        remote,
        "update-ref",
        "refs/heads/extra",
        "refs/heads/main",
    ]);
    env.git(&[
        "--git-dir",
        remote,
        "update-ref",
        "refs/tags/extra",
        "refs/heads/main",
    ]);
    let before = env.git(&["--git-dir", remote, "rev-parse", "refs/heads/main"]);
    let other_refs_before = env.git(&[
        "--git-dir",
        remote,
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        "refs/heads/extra",
        "refs/tags",
    ]);
    let mut push_vars = vars.to_vec();
    push_vars.push(("GIT_CONFIG_PARAMETERS", ""));
    let out = env.run_stdin(
        &push_vars,
        review.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks", "--json"],
    );
    assert_eq!(
        out.status.success(),
        !(diverged || untrusted),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for bytes in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(url));
        assert!(!text.contains(concat!("fixture", "-secret")));
    }
    assert_eq!(fs::read(env.home().join(".a"))?, b"ONE\ntwo\nTHREE\n");
    assert_eq!(fs::read(env.repo().join(".a"))?, b"ONE\ntwo\nthree\n");
    let after = env.git(&["--git-dir", remote, "rev-parse", "refs/heads/main"]);
    assert_eq!(
        env.git(&[
            "--git-dir",
            remote,
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/extra",
            "refs/tags"
        ]),
        other_refs_before
    );
    assert_eq!(
        env.git(&["--git-dir", remote, "for-each-ref", "--format=%(refname)"]),
        "refs/heads/extra\nrefs/heads/main\nrefs/tags/extra\n"
    );
    if diverged || untrusted {
        assert_eq!(before, after);
        assert!(String::from_utf8_lossy(&out.stderr).contains("local commit"));
    } else {
        assert_ne!(before, after);
        assert_eq!(after, env.git(&["-C", repo, "rev-parse", "HEAD"]));
        assert_eq!(
            env.git(&["--git-dir", remote, "show", "main:.a"]),
            "ONE\ntwo\nthree\n"
        );
    }
    Ok(())
}

#[test]
#[ignore = "live loopback transport; run tests/cli_selected/live.sh"]
fn selected_https_verified_loopback_success_and_divergence() -> R {
    for (diverged, untrusted, redirect, wrong_host, directory_conflict, auth) in [
        (false, false, false, false, false, false),
        (true, false, false, false, false, false),
        (true, false, false, false, true, false),
        (false, true, false, false, false, false),
        (false, false, true, false, false, false),
        (false, false, false, true, false, false),
        (false, false, false, false, false, true),
    ] {
        let env = setup();
        let root = env.root();
        fs::write(
            env.home().join(".netrc"),
            format!(
                "machine localhost login synthetic password {}\n",
                concat!("fixture", "-password")
            ),
        )?;
        let cert = tls_certificate(root)?;
        let port = port()?;
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cli_selected/https-server.js");
        let server = Server(
            Command::new("node")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .arg(fixture)
                .arg(root)
                .arg(port.to_string())
                .arg(if redirect {
                    "redirect"
                } else if auth {
                    "auth"
                } else {
                    "normal"
                })
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        wait_port(port)?;
        let host = if wrong_host { "127.0.0.1" } else { "localhost" };
        let url = format!("https://{host}:{port}/remote.git");
        let mut vars = vec![
            ("GIT_SSL_NO_VERIFY", "1"),
            ("HTTPS_PROXY", "http://127.0.0.1:1"),
        ];
        if !untrusted {
            vars.push(("GIT_SSL_CAINFO", cert.to_str().ok_or("cert")?));
        }
        exercise(
            &env,
            &url,
            &vars,
            diverged,
            untrusted || redirect || wrong_host || auth,
            directory_conflict,
        )?;
        assert!(!root.join("unexpected-authorization").exists());
        drop(server);
    }
    Ok(())
}

#[test]
#[ignore = "live loopback transport; run tests/cli_selected/live.sh"]
fn selected_https_uses_only_exact_private_cache_credentials() -> R {
    for (cache_path, insecure, wrong_port) in [
        (Some("remote.git"), false, false),
        (Some("other.git"), false, false),
        (Some("remote.git/sub"), false, false),
        (Some("remote.git"), false, true),
        (Some("remote.git"), true, false),
        (None, false, false),
    ] {
        let env = setup();
        let root = env.root();
        let cert = tls_certificate(root)?;
        let port = port()?;
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cli_selected/https-server.js");
        let server = Server(
            Command::new("node")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .arg(fixture)
                .arg(root)
                .arg(port.to_string())
                .arg("require-auth")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        wait_port(port)?;
        let cache_home = root.join("cache");
        let parent = cache_home.join("git/credential");
        fs::create_dir_all(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let socket = parent.join("socket");
        let cache = Cache(socket.clone());
        let host = if wrong_port {
            "localhost".to_owned()
        } else {
            format!("localhost:{port}")
        };
        if let Some(path) = cache_path {
            let mut store = Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root)
                .args(["credential-cache", "--socket"])
                .arg(&socket)
                .arg("store")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let input = format!(
                "protocol=https\nhost={host}\npath={path}\nusername=fixture\npassword={}\n\n",
                concat!("fixture", "-secret")
            );
            store
                .stdin
                .take()
                .ok_or("cache stdin")?
                .write_all(input.as_bytes())?;
            assert!(store.wait()?.success());
        }
        if insecure {
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o755))?;
        }
        let url = format!("https://localhost:{port}/remote.git");
        let vars = [
            ("GIT_SSL_CAINFO", cert.to_str().ok_or("cert")?),
            ("XDG_CACHE_HOME", cache_home.to_str().ok_or("cache")?),
        ];
        let accepted = cache_path == Some("remote.git") && !insecure && !wrong_port;
        exercise(&env, &url, &vars, false, !accepted, false)?;
        assert_eq!(root.join("authenticated").exists(), accepted);
        if insecure {
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        }
        if let Some(path) = cache_path {
            assert_cache_retained(root, &socket, &host, path)?;
        }
        drop(cache);
        drop(server);
    }
    Ok(())
}

#[test]
#[ignore = "live container transport; run tests/cli_selected/live.sh"]
fn selected_ssh_verified_loopback_success_and_divergence() -> R {
    for (diverged, untrusted, directory_conflict, no_agent) in [
        (false, false, false, false),
        (true, false, false, false),
        (true, false, true, false),
        (false, true, false, false),
        (false, false, false, true),
    ] {
        let env = setup();
        let root = env.root();
        let host = root.join("host-key");
        let identity = root.join("identity");
        for key in [&host, &identity] {
            run(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(key))?;
        }
        let authorized = root.join("authorized_keys");
        fs::copy(identity.with_extension("pub"), &authorized)?;
        let ssh = env.home().join(".ssh");
        fs::create_dir_all(&ssh)?;
        fs::copy(&identity, ssh.join("id_ed25519"))?;
        fs::write(
            ssh.join("config"),
            format!("IdentityFile {}\n", identity.display()),
        )?;
        let port = port()?;
        let host_key = fs::read_to_string(host.with_extension("pub"))?;
        if !untrusted {
            fs::write(
                ssh.join("known_hosts"),
                format!("[127.0.0.1]:{port} {host_key}"),
            )?;
        }
        fs::write(
            root.join("sshd_config"),
            "HostKey /fixture/host-key\nPidFile /run/sshd.pid\nPort 22\nAuthorizedKeysFile /fixture/authorized_keys\nStrictModes no\nUsePAM no\nPermitRootLogin no\nAllowUsers dotman-fixture\nPermitUserRC no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAuthenticationMethods publickey\nDisableForwarding yes\nPermitTTY no\nForceCommand /fixture/receive-only\nSetEnv HOME=/home/dotman-fixture XDG_CONFIG_HOME=/home/dotman-fixture GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null\n",
        )?;
        let service = root.join("receive-only");
        fs::write(
            &service,
            "#!/bin/sh\ncase \"$SSH_ORIGINAL_COMMAND\" in \"git-receive-pack '/fixture/remote.git'\") exec git-receive-pack /fixture/remote.git;; *) exit 1;; esac\n",
        )?;
        fs::set_permissions(&service, fs::Permissions::from_mode(0o755))?;
        let uid = Command::new("id").arg("-u").output()?;
        let uid: u32 = std::str::from_utf8(&uid.stdout)?.trim().parse()?;
        let image = std::env::var("DOTMAN_SSH_FIXTURE_IMAGE")?;
        let server = Container {
            name: format!("dotman-selected-{}-{port}", std::process::id()),
            home: root.to_owned(),
        };
        run(Command::new("docker")
            .env_clear().env("PATH", "/usr/local/bin:/usr/bin:/bin").env("HOME", root)
            .args(["--host", "unix:///var/run/docker.sock", "run", "-d", "--user", "0:0", "--pull=never", "--name"])
            .arg(&server.name)
            .args(["-p", &format!("127.0.0.1:{port}:22"), "--mount", &format!("type=bind,src={},dst=/fixture", root.display()), "--entrypoint", "/bin/sh"])
            .arg(image)
            .args(["-c", &format!("useradd -o -u {uid} -m -d /home/dotman-fixture -s /bin/sh dotman-fixture && passwd -d dotman-fixture >/dev/null && mkdir -p /run/sshd && exec /usr/sbin/sshd -D -e -f /fixture/sshd_config")]))?;
        wait_port(port)?;
        let agent_dir = tempfile::tempdir()?;
        let sock = agent_dir.path().join("agent.sock");
        let agent = Server(
            Command::new("ssh-agent")
                .env_clear()
                .arg("-D")
                .arg("-a")
                .arg(&sock)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        run(Command::new("ssh-add")
            .env_clear()
            .env("SSH_AUTH_SOCK", &sock)
            .arg(&identity))?;
        let url = format!("ssh://dotman-fixture@127.0.0.1:{port}/fixture/remote.git");
        let mut vars = vec![("GIT_SSH_COMMAND", "false")];
        if !no_agent {
            vars.push(("SSH_AUTH_SOCK", sock.to_str().ok_or("socket")?));
        }
        exercise(
            &env,
            &url,
            &vars,
            diverged,
            untrusted || no_agent,
            directory_conflict,
        )?;
        drop(agent);
        drop(server);
    }
    Ok(())
}

#[test]
fn ambiguous_and_rewritten_targets_are_not_offered() -> R {
    let env = setup();
    let repo = env.repo();
    let repo = repo.to_str().ok_or("repo")?;
    for url in [
        "https://user@localhost/repo",
        "https://localhost/repo?x=y",
        "https://localhost/repo#fragment",
        "https://localhost/../repo",
        "https://localhost:0/repo",
        "ssh://-user@localhost/repo",
        "ssh://localhost/repo;command",
        "git@localhost:repo",
        "ext::ssh localhost",
    ] {
        env.git(&["-C", repo, "remote", "set-url", "--push", "origin", url]);
        let output = env.run_with(
            &[("GIT_CONFIG_PARAMETERS", "")],
            &["sync", "--dry-run", "--json"],
        );
        let rows = records(&output)?;
        let outgoing = rows
            .iter()
            .find(|v| v["record"] == "outgoing")
            .ok_or("outgoing")?;
        assert!(outgoing["selected_target"].is_null(), "accepted {url}");
    }
    env.git(&[
        "-C",
        repo,
        "remote",
        "set-url",
        "--push",
        "origin",
        "alias:repo",
    ]);
    env.git(&[
        "-C",
        repo,
        "config",
        "url.https://localhost/.insteadOf",
        "alias:",
    ]);
    let output = env.run_with(
        &[("GIT_CONFIG_PARAMETERS", "")],
        &["sync", "--dry-run", "--json"],
    );
    let rows = records(&output)?;
    let outgoing = rows
        .iter()
        .find(|v| v["record"] == "outgoing")
        .ok_or("outgoing")?;
    assert!(outgoing["selected_target"].is_null());
    Ok(())
}
