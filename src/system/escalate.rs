//! The user's side of `apply --system`: the plan file and the one escalation that runs
//! `system-write` on it.

use super::helper::Plan;
use crate::engine::write::backup_run_dir;
use crate::paths::{create_private_dir, write_atomic};
use crate::proc;
use crate::sys;
use anyhow::{Context as _, Result, anyhow, bail};
use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    None,
    Sudo,
    Doas,
    Run0,
}

impl Tool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sudo => "sudo",
            Self::Doas => "doas",
            Self::Run0 => "run0",
        }
    }

    const fn hint(self, interactive: bool) -> &'static str {
        match (self, interactive) {
            (Self::None, _) => "system-write ran unprivileged, as `escalate = \"none\"` asks",
            (Self::Sudo, true) => "check that your user may run commands with sudo; see sudoers(5)",
            (Self::Sudo, false) => {
                "sudo -n cannot ask for a password; run `sudo -v` in this terminal first, or run dot interactively"
            }
            (Self::Doas, true) => "check that doas.conf(5) permits your user",
            (Self::Doas, false) => "doas -n cannot ask for a password; run dot interactively",
            (Self::Run0, true) => {
                "run0 asks polkit; check that a polkit authentication agent is running"
            }
            (Self::Run0, false) => {
                "run0 --no-ask-password cannot ask polkit for a password; run dot interactively"
            }
        }
    }
}

/// How `system-write` gets started: the tool and where it was found.
pub struct Escalator {
    pub tool: Tool,
    program: Option<PathBuf>,
}

impl Escalator {
    /// `[system] escalate` when set, else the first of sudo, doas and run0 on `PATH`.
    pub fn choose(setting: Option<&str>, config_file: &Path) -> Result<Self> {
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
            (_, true) | (Tool::None, false) => &[],
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

    /// Writes the plan under `state_dir`, runs the helper on it once and removes the plan again.
    pub fn run(&self, state_dir: &Path, plan: &Plan, interactive: bool) -> Result<Ran> {
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .context("cannot find the dot executable")?;
        let (_dir, file) = write_plan(state_dir, plan)?;
        let steps = self.steps(&exe, &file, interactive);
        let Some(((program, args), checks)) = steps.split_last() else {
            bail!("nothing to run");
        };
        let name = self.tool.name();
        for (check, check_args) in checks {
            let status = wait_ignoring_signals(
                proc::command(check)
                    .args(check_args)
                    .spawn()
                    .with_context(|| format!("cannot run {name}"))?,
            )?
            .status;
            if !status.success() {
                bail!("{name} refused ({status}); {}", self.tool.hint(interactive));
            }
        }
        // stdout is a pipe, so run0 passes it through instead of allocating a pseudo TTY.
        let child = proc::command(program)
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot run {name}"))?;
        let output = wait_ignoring_signals(child)?;
        Ok(Ran {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            status: output.status,
            hint: self.tool.hint(interactive),
        })
    }
}

/// What the helper printed and how it, or the escalator in front of it, exited.
pub struct Ran {
    pub stdout: String,
    pub status: ExitStatus,
    pub hint: &'static str,
}

// Ctrl-C at a password prompt reaches dot too; it has to outlive the child to remove the plan.
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
            .steps(Path::new("/x/dot"), Path::new("/p/plan.json"), interactive)
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
        let run = "/x/dot system-write --plan /p/plan.json";
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
        let err = Escalator::choose(Some("pkexec"), config)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("pkexec"), "{err}");
        assert_eq!(
            Escalator::choose(Some("none"), config).unwrap().tool,
            Tool::None
        );
    }
}
