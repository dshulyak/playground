use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossbeam::channel::Sender;
use serde::{Deserialize, Serialize};

use crate::network;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandConfig {
    pub name: String,
    pub command: String,
    pub work_dir: PathBuf,
    pub os_env: Option<BTreeMap<String, String>>,
    pub redirect: bool,
}

#[derive(Debug)]
pub struct Execution {
    pub child: Child,
    pub stdout_handler: Option<JoinHandle<()>>,
    pub stderr_handler: Option<JoinHandle<()>>,
}

pub fn generate(
    prefix: &str,
    redirect: bool,
    per_host: impl Iterator<Item = usize>,
    mut commands: impl Iterator<Item = String>,
    mut env: impl Iterator<Item = BTreeMap<String, String>>,
    mut workdir: impl Iterator<Item = PathBuf>,
) -> Result<Vec<BTreeMap<usize, CommandConfig>>> {
    let mut hosts = vec![];
    for chunk in per_host {
        let mut conf = BTreeMap::new();
        for (index, command) in (0..chunk).zip(&mut commands){
            let work_dir = workdir
                .next()
                .ok_or_else(|| anyhow::anyhow!("workdir is not provided for command {}", index))?;
            let os_env = env.next();
            let command = CommandConfig {
                name: network::Namespace::name(prefix, index),
                command,
                work_dir,
                os_env,
                redirect,
            };
            conf.insert(index, command);
        }
        hosts.push(conf);
    }
    Ok(hosts)
}

pub fn launch(cfg: &BTreeMap<usize, CommandConfig>, execution: &mut BTreeMap<usize, Execution>, errors: &Sender<Result<()>>) -> Result<()> {
    for (index, command) in cfg {
        let (child, stdout_handler, stderr_handler) = launch_one(
            *index,
            &command.name,
            &command.command,
            &command.work_dir,
            &command.os_env,
            command.redirect,
            errors,
        )?;
        let command = Execution {
            child,
            stdout_handler,
            stderr_handler,
        };
        execution.insert(*index, command);
    }
    Ok(())
}

pub fn stop(execution: &mut BTreeMap<usize, Execution>) -> Result<()> {
    for (index, command) in execution.iter_mut() {
        if let Err(err) = kill(&mut command.child) {
            tracing::error!("failed to kill command {}: {:?}", index, err);
        }
    }
    for (index, command) in execution.iter_mut() {
        if let Err(err) = wait(&mut command.child) {
            tracing::error!("failed to wait for command {}: {:?}", index, err);
        }
    }
    execution.clear();
    Ok(())
}

fn kill(process: &mut Child) -> Result<()> {
    process.kill().context("kill process")?;
    Ok(())
}

fn wait(process: &mut Child) -> Result<()> {
    match process.wait() {
        Ok(status) if status.code().is_none() => {
            tracing::debug!("command was terminated by signal: {}", status);
        }
        Ok(status) => {
            if !status.success() {
                anyhow::bail!("command failed with status: {}", status);
            }
        }
        Err(err) => {
            anyhow::bail!("failed to wait for command: {:?}", err);
        }
    }
    // for handler in task.output_handlers.drain(..) {
    //     _ = handler.join();
    // }
    Ok(())
}

fn launch_one(
    index: usize,
    name: &str,
    cmd: &str,
    work_dir: &PathBuf,
    os_env: &Option<BTreeMap<String, String>>,
    redirect: bool,
    errors: &Sender<Result<()>>,
) -> anyhow::Result<(
    Child,
    Option<JoinHandle<()>>,
    Option<JoinHandle<()>>,
)> {
    let cmd = cmd.replace("{index}", &index.to_string());
    let cmd = format!("ip netns exec {} {}", name, cmd);

    tracing::debug!(redirect = redirect, "running command: {}", cmd);

    let mut splitted = cmd.split_whitespace();
    let first = splitted
        .next()
        .ok_or_else(|| anyhow::anyhow!("no command found in the command string: {}", cmd))?;

    let mut shell = Command::new(first);
    shell.args(splitted);
    shell.current_dir(&work_dir);
    if !redirect {
        shell.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        let stdout = OpenOptions::new()
            .append(true)
            .create(true)
            .open(work_dir.join(format!("{}.stdout", name)))?;
        let stderr = OpenOptions::new()
            .append(true)
            .create(true)
            .open(work_dir.join(format!("{}.stderr", name)))?;
        shell.stdout(stdout).stderr(stderr);
    }

    if let Some(os_env) = os_env {
        for (key, value) in os_env {
            shell.env(key, value);
        }
    }

    let mut shell = shell.spawn().context("failed to spawn command")?;
    let handlers = if !redirect {
        let stdout = shell
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stdout from child process"))?;

        let stderr = shell
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stderr from child process"))?;

        let id = name.to_string();
        let sender = errors.clone();
        let stdout_handler = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        tracing::info!("[{}]: {}", id, line);
                    }
                    Err(e) => {
                        let _ = sender.send(Err(e).context("stdout"));
                        return;
                    }
                }
            }
        });
        let id = name.to_string();
        let sender = errors.clone();
        let stderr_handler = thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        tracing::info!("[{}]: {}", id, line);
                    }
                    Err(e) => {
                        let _ = sender.send(Err(e).context("stderr"));
                        return;
                    }
                }
            }
        });
        (Some(stdout_handler), Some(stderr_handler))
    } else {
        (None, None)
    };
    Ok((shell, handlers.0, handlers.1))
}
