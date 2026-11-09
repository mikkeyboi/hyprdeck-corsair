//! Live-mode companion as a systemd --user service (DESIGN §6 "live-mode"). Command execution goes
//! through `Backend::systemctl`, so tests never run systemctl.

use super::backend::Backend;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::Path;

pub const UNIT_NAME: &str = "hyprdeck-corsair.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
}

impl ServiceState {
    pub fn label(self) -> &'static str {
        match self {
            ServiceState::NotInstalled => "Off",
            ServiceState::Stopped => "Off",
            ServiceState::Running => "Running",
        }
    }
}

/// Quote one ExecStart argument: systemd specifiers (`%`) are escaped, paths with spaces quoted.
fn exec_arg(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("The executable path is not valid UTF-8")?;
    if text
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        bail!("The executable path {text:?} cannot be used in a systemd unit");
    }
    let escaped = text.replace('%', "%%");
    Ok(if escaped.contains(char::is_whitespace) {
        format!("\"{escaped}\"")
    } else {
        escaped
    })
}

/// The unit file text for the companion running `exe`.
pub fn unit_text(exe: &Path) -> Result<String> {
    let exe = exec_arg(exe)?;
    Ok(format!(
        "# Written by hyprdeck-corsair (\"Enable live mode\"). Disable live mode in Hyprdeck to stop it.\n\
         [Unit]\n\
         Description=hyprdeck-corsair live-mode companion (Corsair keyboards)\n\
         Documentation=https://github.com/mikkeyboi/hyprdeck-corsair\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} daemon\n\
         ExecStopPost={exe} rescue --all\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

pub fn status(backend: &dyn Backend, daemon_running: bool) -> ServiceState {
    if daemon_running {
        ServiceState::Running
    } else if backend.unit_path().is_ok_and(|path| path.is_file()) {
        ServiceState::Stopped
    } else {
        ServiceState::NotInstalled
    }
}

/// Write the unit atomically (temp file + rename); unchanged content is not rewritten.
fn install(backend: &dyn Backend) -> Result<()> {
    let path = backend.unit_path()?;
    let text = unit_text(&backend.exe()?)?;
    if fs::read_to_string(&path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    let dir = path.parent().context("Unit path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("Creating {}", dir.display()))?;
    let temp = dir.join(format!(".{UNIT_NAME}.{}", std::process::id()));
    let written = (|| -> Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.with_context(|| format!("Writing {}", path.display()))
}

pub fn enable(backend: &dyn Backend) -> Result<()> {
    install(backend)?;
    backend.systemctl(&["daemon-reload"])?;
    backend.systemctl(&["enable", "--now", UNIT_NAME])
}

pub fn disable(backend: &dyn Backend) -> Result<()> {
    if !backend.unit_path()?.is_file() {
        bail!("Live mode is not installed");
    }
    backend.systemctl(&["disable", "--now", UNIT_NAME])
}

pub fn restart(backend: &dyn Backend) -> Result<()> {
    if !backend.unit_path()?.is_file() {
        bail!("Live mode is not installed — use Enable live mode");
    }
    // Pick up a moved/updated executable before restarting.
    install(backend)?;
    backend.systemctl(&["daemon-reload"])?;
    backend.systemctl(&["restart", UNIT_NAME])
}

/// Copy-paste commands for the opt-in input rule (never run by the plugin).
pub const INPUT_RULE_COMMANDS: &str = "sudo install -m 644 71-hyprdeck-corsair-input.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger";

/// Install (or refresh) the unit and start it now and at login. Idempotent.
pub fn ensure_running(backend: &dyn Backend) -> Result<()> {
    install(backend)?;
    backend.systemctl(&["daemon-reload"])?;
    backend.systemctl(&["enable", "--now", UNIT_NAME])
}

/// Stop the helper and keep it from starting at login; a missing unit is fine.
pub fn ensure_stopped(backend: &dyn Backend) -> Result<()> {
    if backend.unit_path()?.is_file() {
        backend.systemctl(&["disable", "--now", UNIT_NAME])?;
    }
    Ok(())
}
