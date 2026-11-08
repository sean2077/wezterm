//! Generic workspace persistence for the mux.
//!
//! The session format deliberately knows nothing about Claude, Codex or any
//! other agent.  A pane carries its original spawn argv/cwd/env, its user vars
//! and its title.  Agent integrations communicate through user vars:
//!
//! * `agent.restore_argv` - JSON array; a complete replacement command used on
//!   restore.  This var is host-only: OSC 1337 cannot set `agent.*` names, so
//!   untrusted terminal output cannot inject a command.
//! * `agent.shutdown`     - non-empty means close-agent-sessions should target it
//!
//! `agent.resume_args` is obsolete.  It was appended to the original spawn
//! argv, which sent the arguments to shell wrappers instead of the agent; it is
//! ignored on restore (with a warning) so old snapshots stay readable.
//!
//! The session file still has an `env` field for format compatibility, but
//! spawn-info env is not captured today, so it is always empty.
//!
//! This keeps the native implementation reusable for any terminal program.

use crate::pane::{CachePolicy, PaneId};
use crate::Mux;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use config::keyassignment::SpawnTabDomain;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const AGENT_RESTORE_ARGV: &str = "agent.restore_argv";
/// Obsolete append-style resume variable; ignored on restore.
pub const AGENT_RESUME_ARGS: &str = "agent.resume_args";
pub const AGENT_SHUTDOWN: &str = "agent.shutdown";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnInfo {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneState {
    pub cwd: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub title: String,
    #[serde(default)]
    pub user_vars: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabState {
    pub title: String,
    pub active_tab: usize,
    pub pane: PaneState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowState {
    pub workspace: String,
    pub active_tab: usize,
    pub tabs: Vec<TabState>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionState {
    pub version: u32,
    pub created_at: String,
    #[serde(default)]
    pub windows: Vec<WindowState>,
}

#[derive(Clone, Debug)]
pub struct SaveSummary {
    pub path: PathBuf,
    pub windows: usize,
    pub tabs: usize,
    /// Tabs that could not be represented because their pane has no local
    /// spawn command (for example ssh or tmux client panes).
    pub skipped: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RestoreSummary {
    pub windows: usize,
    pub tabs: usize,
    pub failed: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CloseSummary {
    pub closed: usize,
    pub remaining: usize,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            version: 1,
            created_at: Utc::now().to_rfc3339(),
            windows: Vec::new(),
        }
    }
}

pub fn default_workspace_path() -> PathBuf {
    config::DATA_DIR.join("workspace.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv_of(command: &RestoreCommand) -> Vec<String> {
        command
            .command
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>()
    }

    fn pane_with_vars(argv: &[&str], vars: &[(&str, &str)]) -> PaneState {
        PaneState {
            cwd: Some("/tmp".to_string()),
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
            env: BTreeMap::new(),
            title: "agent".to_string(),
            user_vars: vars
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn restore_argv_replaces_the_command() {
        let pane = pane_with_vars(
            &["zsh", "-i"],
            &[(AGENT_RESTORE_ARGV, r#"["zsh","-i","-c","claude --resume 123"]"#)],
        );
        let command = command_for(&pane).unwrap();
        assert!(command.used_restore_argv);
        assert_eq!(
            argv_of(&command),
            vec!["zsh", "-i", "-c", "claude --resume 123"]
        );
        // The recorded base argv must stay the original spawn command.
        assert_eq!(command.base_argv, vec!["zsh", "-i"]);
    }

    #[test]
    fn obsolete_resume_args_are_ignored() {
        let pane = pane_with_vars(
            &["agent", "--flag"],
            &[(AGENT_RESUME_ARGS, r#"["--resume","123"]"#)],
        );
        let command = command_for(&pane).unwrap();
        assert!(!command.used_restore_argv);
        assert_eq!(argv_of(&command), vec!["agent", "--flag"]);
    }

    #[test]
    fn invalid_restore_argv_falls_back_to_the_base_argv() {
        let pane = pane_with_vars(&["sh"], &[(AGENT_RESTORE_ARGV, "not json")]);
        let command = command_for(&pane).unwrap();
        assert!(!command.used_restore_argv);
        assert_eq!(argv_of(&command), vec!["sh"]);
    }

    #[test]
    fn empty_argv_uses_default_shell() {
        let pane = pane_with_vars(&[], &[]);
        assert!(command_for(&pane).is_none());
    }

    #[test]
    fn session_state_round_trips() {
        let state = SessionState {
            version: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            windows: vec![WindowState {
                workspace: "default".to_string(),
                active_tab: 0,
                tabs: vec![TabState {
                    title: "tab".to_string(),
                    active_tab: 0,
                    pane: PaneState {
                        cwd: Some("/tmp".to_string()),
                        argv: vec!["sh".to_string()],
                        env: BTreeMap::new(),
                        title: "sh".to_string(),
                        user_vars: BTreeMap::new(),
                    },
                }],
            }],
        };
        let encoded = serde_json::to_string(&state).unwrap();
        let decoded: SessionState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(state, decoded);
    }
}

fn path_string(url: &url::Url) -> Option<String> {
    url.to_file_path()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .or_else(|| {
            let path = url.path();
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
}

fn write_json(path: &Path, state: &SessionState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        let _ = std::fs::copy(path, backup);
    }
    let temp = PathBuf::from(format!("{}.tmp-{}", path.display(), std::process::id()));
    let data = serde_json::to_vec_pretty(state).context("serializing workspace")?;
    std::fs::write(&temp, data).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("renaming {} to {}", temp.display(), path.display()))?;
    Ok(())
}

fn read_json(path: &Path) -> Result<SessionState> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let state: SessionState = serde_json::from_slice(&data)
        .with_context(|| format!("parsing workspace {}", path.display()))?;
    if state.version != 1 {
        return Err(anyhow!(
            "unsupported workspace version {} in {}",
            state.version,
            path.display()
        ));
    }
    Ok(state)
}

fn truthy(value: Option<&String>) -> bool {
    matches!(value.map(|v| v.as_str()), Some("1" | "true" | "yes"))
}

fn restore_argv(user_vars: &BTreeMap<String, String>) -> Option<Vec<String>> {
    let raw = user_vars.get(AGENT_RESTORE_ARGV)?;
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(argv) if !argv.is_empty() && !argv[0].is_empty() => Some(argv),
        Ok(_) => {
            log::warn!("restore workspace: ignoring empty {AGENT_RESTORE_ARGV}");
            None
        }
        Err(err) => {
            log::warn!("restore workspace: ignoring invalid {AGENT_RESTORE_ARGV}: {err:#}");
            None
        }
    }
}

struct RestoreCommand {
    command: portable_pty::CommandBuilder,
    /// The pane's original spawn argv.  Restored panes must record this rather
    /// than the replacement command so repeated save/restore cycles do not
    /// accumulate arguments.
    base_argv: Vec<String>,
    used_restore_argv: bool,
}

fn command_for(pane: &PaneState) -> Option<RestoreCommand> {
    if pane.user_vars.contains_key(AGENT_RESUME_ARGS) {
        log::warn!(
            "restore workspace: ignoring obsolete {AGENT_RESUME_ARGS} (use {AGENT_RESTORE_ARGV}): argv={:?}",
            pane.argv
        );
    }
    let replacement = restore_argv(&pane.user_vars);
    let used_restore_argv = replacement.is_some();
    let argv = replacement.unwrap_or_else(|| pane.argv.clone());
    if argv.is_empty() {
        return None;
    }
    let mut command = portable_pty::CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    if let Some(cwd) = &pane.cwd {
        command.cwd(cwd);
    }
    for (key, value) in &pane.env {
        command.env(key, value);
    }
    Some(RestoreCommand {
        command,
        base_argv: pane.argv.clone(),
        used_restore_argv,
    })
}

impl Mux {
    pub fn save_workspace(&self, path: &Path) -> Result<SaveSummary> {
        let mut state = SessionState::new();
        let mut window_ids = self.iter_windows();
        window_ids.sort();
        let mut skipped = 0usize;

        for window_id in window_ids {
            let window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window {window_id} disappeared while saving"))?;
            let mut tabs = Vec::new();
            for tab in window.iter_tabs() {
                let Some(pane) = tab.get_active_pane() else {
                    continue;
                };
                let Some(spawn) = pane.get_spawn_info() else {
                    // A pane hosted by another domain (ssh, tmux, ...) has no
                    // local spawn command.  Restoring it as the default shell
                    // would silently replace the remote session, so skip it.
                    skipped += 1;
                    log::warn!(
                        "save workspace: skipping pane {} ({:?}); it has no local spawn command",
                        pane.pane_id(),
                        tab.get_title()
                    );
                    continue;
                };
                let cwd = pane
                    .get_current_working_dir(CachePolicy::AllowStale)
                    .as_ref()
                    .and_then(path_string)
                    .or_else(|| spawn.cwd.clone());
                let user_vars = pane
                    .copy_user_vars()
                    .into_iter()
                    .collect::<BTreeMap<_, _>>();
                tabs.push(TabState {
                    title: tab.get_title(),
                    // Kept for format compatibility; the active tab lives on
                    // WindowState.  Per-tab split focus was never restored.
                    active_tab: 0,
                    pane: PaneState {
                        cwd,
                        argv: spawn.argv.clone(),
                        env: spawn.env.clone(),
                        title: pane.get_title(),
                        user_vars,
                    },
                });
            }
            if tabs.is_empty() {
                continue;
            }
            state.windows.push(WindowState {
                workspace: window.get_workspace().to_string(),
                active_tab: window.get_active_tab_idx(),
                tabs,
            });
        }

        if skipped > 0 {
            log::warn!(
                "save workspace: skipped {skipped} pane(s) without a local spawn command (ssh/tmux)"
            );
        }
        write_json(path, &state)?;
        Ok(SaveSummary {
            path: path.to_path_buf(),
            windows: state.windows.len(),
            tabs: state.windows.iter().map(|window| window.tabs.len()).sum(),
            skipped,
        })
    }

    pub async fn restore_workspace(
        &self,
        path: &Path,
        target_window: Option<crate::window::WindowId>,
        replace: bool,
    ) -> Result<RestoreSummary> {
        let state = read_json(path)?;
        let target_window = match target_window {
            Some(id) => Some(id),
            None => {
                let windows = self.iter_windows();
                if windows.len() == 1 {
                    windows.first().copied()
                } else if replace {
                    return Err(anyhow!(
                        "restore-workspace --replace requires --window-id when more than one window exists"
                    ));
                } else {
                    None
                }
            }
        };
        let old_tabs: Vec<_> = if replace {
            target_window
                .and_then(|id| self.get_window(id))
                .map(|window| window.iter_tabs().map(|tab| tab.tab_id()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut summary = RestoreSummary::default();
        let mut restored_any = false;
        // Active tabs are applied after --replace removes the old tabs, so the
        // index still points at the same restored tab.
        let mut active_targets: Vec<(crate::window::WindowId, crate::tab::TabId)> = Vec::new();
        for (window_index, saved_window) in state.windows.iter().enumerate() {
            let mut current_window = if window_index == 0 {
                target_window
            } else {
                None
            };
            let mut restored_tabs: Vec<(usize, crate::tab::TabId)> = Vec::new();
            for (tab_index, saved_tab) in saved_window.tabs.iter().enumerate() {
                // No effective command at all (for example the GUI's default
                // startup pane, whose spawn argv is empty) means the default
                // shell; that is what the pane was running too.
                let (command, used_restore_argv, base_argv) = match command_for(&saved_tab.pane) {
                    Some(restore) => (
                        Some(restore.command),
                        restore.used_restore_argv,
                        restore.base_argv,
                    ),
                    None => {
                        log::debug!(
                            "restore workspace: tab {tab_index} ({:?}) has no spawn argv; using the default shell",
                            saved_tab.title
                        );
                        (None, false, Vec::new())
                    }
                };
                // The tab bar size comes from the target window; only a brand
                // new window needs the configured initial size.
                let size = current_window
                    .and_then(|window_id| {
                        self.get_window(window_id)
                            .and_then(|window| window.get_active_tab().map(|tab| tab.get_size()))
                    })
                    .unwrap_or_else(|| config::configuration().initial_size(0, None));
                let workspace = if saved_window.workspace.is_empty() {
                    "default".to_string()
                } else {
                    saved_window.workspace.clone()
                };
                let result = self
                    .spawn_tab_or_window(
                        current_window,
                        SpawnTabDomain::DefaultDomain,
                        command,
                        saved_tab.pane.cwd.clone(),
                        size,
                        None,
                        workspace,
                        None,
                    )
                    .await;
                match result {
                    Ok((tab, pane, window_id)) => {
                        tab.set_title(&saved_tab.title);
                        if used_restore_argv && !base_argv.is_empty() {
                            // Remember the pane's original spawn command, not
                            // the replacement command, so repeated restore
                            // cycles never accumulate arguments.
                            pane.set_spawn_info(Some(SpawnInfo {
                                argv: base_argv,
                                cwd: saved_tab.pane.cwd.clone(),
                                env: BTreeMap::new(),
                            }));
                        }
                        if current_window.is_none() {
                            current_window = Some(window_id);
                        }
                        restored_tabs.push((tab_index, tab.tab_id()));
                        summary.tabs += 1;
                        restored_any = true;
                    }
                    Err(err) => {
                        log::error!(
                            "restore workspace: window {window_index} tab {tab_index} failed: {:#}",
                            err
                        );
                        summary.failed += 1;
                    }
                }
            }
            if !restored_tabs.is_empty() {
                // Prefer the tab that was active when saved; tabs before it
                // may have failed, so match by the original index and fall
                // back to the last restored tab.
                let desired = restored_tabs
                    .iter()
                    .find(|(saved_idx, _)| *saved_idx == saved_window.active_tab)
                    .map(|(_, tab_id)| *tab_id)
                    .or_else(|| restored_tabs.last().map(|(_, tab_id)| *tab_id));
                if let (Some(window_id), Some(tab_id)) = (current_window, desired) {
                    active_targets.push((window_id, tab_id));
                }
                summary.windows += 1;
            }
        }

        // Never destroy the target window when nothing could be restored.
        if replace && restored_any {
            for tab_id in old_tabs {
                self.remove_tab(tab_id);
            }
        }
        for (window_id, tab_id) in active_targets {
            if let Some(mut window) = self.get_window_mut(window_id) {
                let idx = window
                    .iter_tabs()
                    .position(|tab| tab.tab_id() == tab_id);
                if let Some(idx) = idx {
                    window.remember_and_set_active_tab_idx(idx);
                }
            }
        }
        self.prune_dead_windows();
        Ok(summary)
    }

    pub async fn close_agent_sessions(&self, timeout: Duration) -> Result<CloseSummary> {
        // Defend against absurd values from older clients even though the CLI
        // already validates the range.
        let timeout = timeout
            .max(Duration::from_secs(1))
            .min(Duration::from_secs(3600));
        let mut targets = Vec::new();
        for window_id in self.iter_windows() {
            let Some(window) = self.get_window(window_id) else {
                continue;
            };
            for tab in window.iter_tabs() {
                for positioned in tab.iter_panes_ignoring_zoom() {
                    let pane = positioned.pane;
                    if truthy(pane.copy_user_vars().get(AGENT_SHUTDOWN)) {
                        targets.push(pane);
                    }
                }
            }
        }
        if targets.is_empty() {
            return Ok(CloseSummary::default());
        }

        // Send each step to every pane before moving on, so a room full of
        // agents sees the same sequence in parallel rather than one pane
        // waiting for the previous pane's full 450ms.
        for pane in &targets {
            let mut writer = pane.writer();
            let _ = writer.write_all(b"\x1b");
            let _ = writer.flush();
        }
        smol::Timer::after(Duration::from_millis(200)).await;
        for pane in &targets {
            let mut writer = pane.writer();
            let _ = writer.write_all(b"\x03");
            let _ = writer.flush();
        }
        smol::Timer::after(Duration::from_millis(250)).await;
        for pane in &targets {
            let mut writer = pane.writer();
            let _ = writer.write_all(b"/exit\r");
            let _ = writer.flush();
        }

        let deadline = Instant::now() + timeout;
        loop {
            let remaining: Vec<_> = targets
                .iter()
                .filter(|pane| {
                    self.get_pane(pane.pane_id()).is_some()
                        && truthy(pane.copy_user_vars().get(AGENT_SHUTDOWN))
                })
                .collect();
            if remaining.is_empty() {
                return Ok(CloseSummary {
                    closed: targets.len(),
                    remaining: 0,
                });
            }
            if Instant::now() >= deadline {
                let remaining_count = remaining.len();
                return Ok(CloseSummary {
                    closed: targets.len() - remaining_count,
                    remaining: remaining_count,
                });
            }
            smol::Timer::after(Duration::from_millis(500)).await;
        }
    }

    pub fn set_user_var(&self, pane_id: PaneId, name: &str, value: Option<&str>) -> Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        pane.set_user_var(name, value);
        // The OSC 1337 path reports changes through the terminal alert
        // handler; the host-side path must do the same so Lua events fire and
        // the GUI repaints immediately.
        self.notify(crate::MuxNotification::Alert {
            pane_id,
            alert: wezterm_term::Alert::SetUserVar {
                name: name.to_string(),
                value: value.unwrap_or("").to_string(),
            },
        });
        Ok(())
    }
}
