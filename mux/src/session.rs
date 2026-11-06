//! Generic workspace persistence for the mux.
//!
//! The session format deliberately knows nothing about Claude, Codex or any
//! other agent.  A pane carries its original spawn argv/cwd/env, its user vars
//! and its title.  Agent integrations communicate through user vars:
//!
//! * `agent.resume_args` - JSON array of arguments appended on restore
//! * `agent.shutdown`    - non-empty means close-agent-sessions should target it
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
use wezterm_term::TerminalSize;

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

    #[test]
    fn command_appends_generic_resume_args() {
        let pane = PaneState {
            cwd: Some("/tmp".to_string()),
            argv: vec!["agent".to_string(), "--flag".to_string()],
            env: BTreeMap::new(),
            title: "agent".to_string(),
            user_vars: BTreeMap::from([(
                AGENT_RESUME_ARGS.to_string(),
                r#"["--resume","123"]"#.to_string(),
            )]),
        };
        let command = command_for(&pane).unwrap();
        assert_eq!(
            command
                .get_argv()
                .iter()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>(),
            vec!["agent", "--flag", "--resume", "123"]
        );
    }

    #[test]
    fn empty_argv_uses_default_shell() {
        let pane = PaneState {
            cwd: None,
            argv: vec![],
            env: BTreeMap::new(),
            title: String::new(),
            user_vars: BTreeMap::new(),
        };
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

fn resume_args(user_vars: &BTreeMap<String, String>) -> Vec<String> {
    user_vars
        .get(AGENT_RESUME_ARGS)
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
}

fn command_for(pane: &PaneState) -> Option<portable_pty::CommandBuilder> {
    let mut argv = pane.argv.clone();
    argv.extend(resume_args(&pane.user_vars));
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
    Some(command)
}

impl Mux {
    pub fn save_workspace(&self, path: &Path) -> Result<SaveSummary> {
        let mut state = SessionState::new();
        let mut window_ids = self.iter_windows();
        window_ids.sort();

        for window_id in window_ids {
            let window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window {window_id} disappeared while saving"))?;
            let mut tabs = Vec::new();
            for tab in window.iter_tabs() {
                let Some(pane) = tab.get_active_pane() else {
                    continue;
                };
                let spawn = pane.get_spawn_info();
                let cwd = pane
                    .get_current_working_dir(CachePolicy::AllowStale)
                    .as_ref()
                    .and_then(path_string)
                    .or_else(|| spawn.as_ref().and_then(|info| info.cwd.clone()));
                let user_vars = pane
                    .copy_user_vars()
                    .into_iter()
                    .collect::<BTreeMap<_, _>>();
                tabs.push(TabState {
                    title: tab.get_title(),
                    active_tab: 0,
                    pane: PaneState {
                        cwd,
                        argv: spawn
                            .as_ref()
                            .map(|info| info.argv.clone())
                            .unwrap_or_default(),
                        env: spawn
                            .as_ref()
                            .map(|info| info.env.clone())
                            .unwrap_or_default(),
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

        write_json(path, &state)?;
        Ok(SaveSummary {
            path: path.to_path_buf(),
            windows: state.windows.len(),
            tabs: state.windows.iter().map(|window| window.tabs.len()).sum(),
        })
    }

    pub async fn restore_workspace(
        &self,
        path: &Path,
        target_window: Option<crate::window::WindowId>,
        replace: bool,
    ) -> Result<RestoreSummary> {
        let state = read_json(path)?;
        let target_window = target_window.or_else(|| {
            let windows = self.iter_windows();
            if windows.len() == 1 {
                windows.first().copied()
            } else {
                None
            }
        });
        let old_tabs: Vec<_> = if replace {
            target_window
                .and_then(|id| self.get_window(id))
                .map(|window| window.iter_tabs().map(|tab| tab.tab_id()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut summary = RestoreSummary::default();
        for (window_index, saved_window) in state.windows.iter().enumerate() {
            let mut current_window = if window_index == 0 {
                target_window
            } else {
                None
            };
            let mut active_tab = 0;
            for (tab_index, saved_tab) in saved_window.tabs.iter().enumerate() {
                let window_id_for_spawn = if tab_index == 0 {
                    current_window
                } else {
                    current_window
                };
                let command = command_for(&saved_tab.pane);
                let size = TerminalSize::default();
                let workspace = if saved_window.workspace.is_empty() {
                    "default".to_string()
                } else {
                    saved_window.workspace.clone()
                };
                let result = self
                    .spawn_tab_or_window(
                        window_id_for_spawn,
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
                        for (name, value) in &saved_tab.pane.user_vars {
                            pane.set_user_var(name, Some(value));
                        }
                        if tab_index == 0 {
                            current_window = Some(window_id);
                        }
                        active_tab = tab_index;
                        summary.tabs += 1;
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
            if let Some(window_id) = current_window {
                if let Some(mut window) = self.get_window_mut(window_id) {
                    let count = window.count_tabs();
                    if count > 0 {
                        window.remember_and_set_active_tab_idx(active_tab.min(count - 1));
                    }
                }
                summary.windows += 1;
            }
        }

        for tab_id in old_tabs {
            self.remove_tab(tab_id);
        }
        self.prune_dead_windows();
        Ok(summary)
    }

    pub async fn close_agent_sessions(
        &self,
        timeout: Duration,
        force: bool,
    ) -> Result<CloseSummary> {
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

        for pane in &targets {
            {
                let mut writer = pane.writer();
                let _ = writer.write_all(b"\x1b");
                let _ = writer.flush();
            }
            smol::Timer::after(Duration::from_millis(200)).await;
            {
                let mut writer = pane.writer();
                let _ = writer.write_all(b"\x03");
                let _ = writer.flush();
            }
            smol::Timer::after(Duration::from_millis(250)).await;
            {
                let mut writer = pane.writer();
                let _ = writer.write_all(b"/exit\r");
                let _ = writer.flush();
            }
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
            if force {
                // Continue a little while even with force, so a session that
                // is already exiting can finish its hook.
            }
            smol::Timer::after(Duration::from_millis(500)).await;
        }
    }

    pub fn set_user_var(&self, pane_id: PaneId, name: &str, value: Option<&str>) -> Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        pane.set_user_var(name, value);
        Ok(())
    }
}
