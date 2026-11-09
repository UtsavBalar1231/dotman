//! The user's side of `apply --system`: the plan file and the one escalation that runs
//! `system-write` on it.

use super::helper::Plan;
use crate::engine::write::backup_run_dir;
use crate::name;
use crate::paths::{create_private_dir, write_atomic};
use crate::proc;
use crate::sys;
use anyhow::{Context as _, Result, anyhow, bail};
use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

// Where sudo, run0 and doas name the user who called them.
const ESCALATOR_VARS: [&str; 4] = ["SUDO_UID", "SUDO_USER", "SUDO_GID", "DOAS_USER"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    None,
    Sudo,
    Doas,
    Run0,
    /// dotman already runs as root, so the helper starts without an escalator.
    Root,
}

impl Tool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sudo => "sudo",
            Self::Doas => "doas",
            Self::Run0 => "run0",
            Self::Root => "root",
        }
    }

    fn hint(self, interactive: bool) -> String {
        let bin = name::bin();
        match (self, interactive) {
            (Self::None, _) => {
                "system-write ran unprivileged, as `escalate = \"none\"` asks".into()
            }
            (Self::Root, _) => {
                format!("{bin} runs as root, so system-write ran without an escalator")
            }
            (Self::Sudo, true) => {
                "check that your user may run commands with sudo; see sudoers(5)".into()
            }
            (Self::Sudo, false) => format!(
                "sudo -n cannot ask for a password; run `sudo -v` in this terminal first, or run {bin} interactively"
            ),
            (Self::Doas, true) => "check that doas.conf(5) permits your user".into(),
            (Self::Doas, false) => {
                format!("doas -n cannot ask for a password; run {bin} interactively")
            }
            (Self::Run0, true) => {
                "run0 asks polkit; check that a polkit authentication agent is running".into()
            }
            (Self::Run0, false) => format!(
                "run0 --no-ask-password cannot ask polkit for a password; run {bin} interactively"
            ),
        }
    }
}

/// How `system-write` gets started: the tool and where it was found.
pub struct Escalator {
    pub tool: Tool,
    program: Option<PathBuf>,
}

impl Escalator {
    /// No escalator when `euid` is root; else `[system] escalate` when set, else the first of
    /// sudo, doas and run0 on `PATH`.
    pub fn choose(setting: Option<&str>, config_file: &Path, euid: u32) -> Result<Self> {
        if euid == 0 {
            return Ok(Self {
                tool: Tool::Root,
                program: None,
            });
        }
        let config = config_file.display();
        let tool = match setting {
            None => {
                return [Tool::Sudo, Tool::Doas, Tool::Run0]
                    .into_iter()
                    .find_map(|tool| {
                        proc::find_program(tool.name()).map(|program| Self {
                            tool,
                            program: Some(program),
                        })
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "no sudo, doas or run0 is on PATH to write system files with; install one, or name one as `escalate` under [system] in {config}"
                        )
                    });
            }
            Some("none") => Tool::None,
            Some("sudo") => Tool::Sudo,
            Some("doas") => Tool::Doas,
            Some("run0") => Tool::Run0,
            Some(other) => bail!(
                "`escalate = \"{other}\"` under [system] in {config} is not one of none, sudo, doas and run0"
            ),
        };
        let program = match tool {
            Tool::None => None,
            _ => Some(proc::find_program(tool.name()).ok_or_else(|| {
                anyhow!(
                    "[system] escalate in {config} names {}, which is not on PATH",
                    tool.name()
                )
            })?),
        };
        Ok(Self { tool, program })
    }

    /// The commands to run in order. sudo checks the credential first, so a refused password
    /// stops the run before the helper starts.
    fn steps(&self, exe: &Path, plan: &Path, interactive: bool) -> Vec<(PathBuf, Vec<OsString>)> {
        let helper: Vec<OsString> = vec!["system-write".into(), "--plan".into(), plan.into()];
        let Some(program) = &self.program else {
            return vec![(exe.to_path_buf(), helper)];
        };
        let quiet: &[&str] = match (self.tool, interactive) {
            (_, true) | (Tool::None | Tool::Root, false) => &[],
            (Tool::Sudo | Tool::Doas, false) => &["-n"],
            // run0(1), systemd 262: `--no-ask-password` does not query for authentication;
            // `-n`/`--non-interactive` is documented as equivalent but only exists since 262.
            (Tool::Run0, false) => &["--no-ask-password"],
        };
        let with = |tail: Vec<OsString>| {
            let mut args: Vec<OsString> = quiet.iter().map(OsString::from).collect();
            args.extend(tail);
            (program.clone(), args)
        };
        let mut steps = Vec::new();
        if self.tool == Tool::Sudo {
            steps.push(with(vec!["-v".into()]));
        }
        let mut tail: Vec<OsString> = vec!["--".into(), exe.into()];
        tail.extend(helper);
        steps.push(with(tail));
        steps
    }

    fn command(&self, program: &Path, args: &[OsString]) -> Command {
        let mut command = proc::command(program);
        command.args(args);
        if self.tool == Tool::Root {
            // A root shell entered with `sudo -i` still names the user here, and the helper would
            // then expect that user, not root, to own the plan.
            for name in ESCALATOR_VARS {
                command.env_remove(name);
            }
        }
        command
    }

    /// Writes the plan under `state_dir`, runs the helper on it once and removes the plan again.
    pub fn run(&self, state_dir: &Path, plan: &Plan, interactive: bool) -> Result<Ran> {
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .with_context(|| format!("cannot find the {} executable", name::bin()))?;
        let (_dir, file) = write_plan(state_dir, plan)?;
        let steps = self.steps(&exe, &file, interactive);
        let Some(((program, args), checks)) = steps.split_last() else {
            bail!("nothing to run");
        };
        let name = self.tool.name();
        for (check, check_args) in checks {
            let status = wait_ignoring_signals(
                self.command(check, check_args)
                    .spawn()
                    .with_context(|| format!("cannot run {name}"))?,
            )?
            .status;
            if !status.success() {
                bail!("{name} refused ({status}); {}", self.tool.hint(interactive));
            }
        }
        // stdout is a pipe, so run0 passes it through instead of allocating a pseudo TTY.
        let mut command = self.command(program, args);
        command.stdout(Stdio::piped());
        // Only what run0 prints tells a missing systemd from a polkit refusal. Its polkit agent
        // prompts on the terminal, not on stderr.
        if self.tool == Tool::Run0 {
            command.stderr(Stdio::piped());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {name}"))?;
        let relayed = child.stderr.take().map(relay);
        let output = wait_ignoring_signals(child)?;
        let said = relayed
            .and_then(|relay| relay.join().ok())
            .unwrap_or_default();
        let hint = if self.tool == Tool::Run0 && !output.status.success() && no_systemd(&said) {
            RUN0_NEEDS_SYSTEMD.to_owned()
        } else {
            self.tool.hint(interactive)
        };
        Ok(Ran {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            status: output.status,
            hint,
        })
    }
}

const RUN0_NEEDS_SYSTEMD: &str =
    "run0 needs systemd as PID 1; use sudo or doas, or set `[system] escalate`";

/// How much of the end of run0's stderr is kept to look for the reason it failed.
const KEPT: usize = 4096;

/// Copies `pipe` to stderr as it arrives and returns the last [`KEPT`] bytes.
fn relay(mut pipe: ChildStderr) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let (mut tail, mut buf) = (Vec::new(), [0_u8; 1024]);
        let mut err = io::stderr();
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = buf.get(..n).unwrap_or_default();
                    let _ = err.write_all(chunk);
                    tail.extend_from_slice(chunk);
                    tail.drain(..tail.len().saturating_sub(KEPT));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        tail
    })
}

// What systemd prints when PID 1 is not systemd, as in a container: "System has not been booted
// with systemd as init system (PID 1)", then "Failed to connect to ... bus ...: Host is down".
fn no_systemd(said: &[u8]) -> bool {
    let said = String::from_utf8_lossy(said);
    said.contains("not been booted with systemd") || said.contains("Host is down")
}

/// What the helper printed and how it, or the escalator in front of it, exited.
pub struct Ran {
    pub stdout: String,
    pub status: ExitStatus,
    pub hint: String,
}

// Ctrl-C at a password prompt reaches dotman too; it has to outlive the child to remove the plan.
fn wait_ignoring_signals(child: std::process::Child) -> Result<std::process::Output> {
    let _ignored = sys::ignore_signals(&[SIGINT, SIGQUIT, SIGHUP, SIGTERM]);
    child
        .wait_with_output()
        .context("cannot wait for the escalation")
}

/// The plan directory, removed with the plan when dropped, whatever the helper did.
struct PlanDir(PathBuf);

impl Drop for PlanDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_plan(state_dir: &Path, plan: &Plan) -> Result<(PlanDir, PathBuf)> {
    let parent = state_dir.join("system-plan");
    create_private_dir(&parent, false)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let dir = backup_run_dir(&parent, SystemTime::now())?;
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let guard = PlanDir(dir.clone());
    let file = dir.join("plan.json");
    write_atomic(&file, &serde_json::to_vec_pretty(plan)?, 0o600)?;
    Ok((guard, file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tool: Tool, interactive: bool) -> Vec<Vec<String>> {
        let escalator = Escalator {
            tool,
            program: (tool != Tool::None).then(|| PathBuf::from(tool.name())),
        };
        escalator
            .steps(
                Path::new("/x/dotman"),
                Path::new("/p/plan.json"),
                interactive,
            )
            .into_iter()
            .map(|(program, args)| {
                std::iter::once(program.display().to_string())
                    .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn each_tool_gets_its_own_flags() {
        let run = "/x/dotman system-write --plan /p/plan.json";
        let shown = |tool, interactive| -> Vec<String> {
            argv(tool, interactive)
                .iter()
                .map(|step| step.join(" "))
                .collect()
        };
        assert_eq!(
            shown(Tool::Sudo, true),
            ["sudo -v".to_owned(), format!("sudo -- {run}")]
        );
        assert_eq!(
            shown(Tool::Sudo, false),
            ["sudo -n -v".to_owned(), format!("sudo -n -- {run}")]
        );
        assert_eq!(shown(Tool::Doas, true), [format!("doas -- {run}")]);
        assert_eq!(shown(Tool::Doas, false), [format!("doas -n -- {run}")]);
        assert_eq!(shown(Tool::Run0, true), [format!("run0 -- {run}")]);
        assert_eq!(
            shown(Tool::Run0, false),
            [format!("run0 --no-ask-password -- {run}")]
        );
        assert_eq!(shown(Tool::None, false), [run.to_owned()]);
    }

    #[test]
    fn an_unknown_escalator_is_refused() {
        let config = Path::new("/c/config.toml");
        let err = Escalator::choose(Some("pkexec"), config, 1000)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("pkexec"), "{err}");
        assert_eq!(
            Escalator::choose(Some("none"), config, 1000).unwrap().tool,
            Tool::None
        );
    }

    #[test]
    fn root_runs_the_helper_directly_without_the_escalators_variables() {
        let config = Path::new("/c/config.toml");
        for setting in [None, Some("sudo"), Some("none"), Some("pkexec")] {
            let escalator = Escalator::choose(setting, config, 0).unwrap();
            assert_eq!(escalator.tool, Tool::Root, "{setting:?}");
            let steps = escalator.steps(Path::new("/x/dotman"), Path::new("/p/plan.json"), false);
            let [(program, args)] = steps.as_slice() else {
                unreachable!("one step: {steps:?}");
            };
            assert_eq!(program, Path::new("/x/dotman"));
            assert_eq!(args, &["system-write", "--plan", "/p/plan.json"]);
            let command = escalator.command(program, args);
            for name in ESCALATOR_VARS {
                let removed = command
                    .get_envs()
                    .any(|(key, value)| key == name && value.is_none());
                assert!(removed, "{name}");
            }
        }
        let user = Escalator::choose(Some("none"), config, 1000).unwrap();
        let command = user.command(Path::new("/x/dotman"), &[]);
        assert!(
            !command
                .get_envs()
                .any(|(key, _)| ESCALATOR_VARS.iter().any(|name| key == *name))
        );
    }
}
