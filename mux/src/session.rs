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
//!
//! Version 3 records the active workspace and local domain alongside each pane's
//! pre-fixup command. Version 2 layouts and version 1 single-pane tabs are still
//! readable. Their already-adapted commands replay in the host domain.

use crate::domain::SplitSource;
use crate::localpane::LocalPane;
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::tab::{SplitDirection, SplitRequest, SplitSize, TabLayoutNode};
use crate::Mux;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use config::keyassignment::SpawnTabDomain;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_term::TerminalSize;

pub const AGENT_RESTORE_ARGV: &str = "agent.restore_argv";
/// Obsolete append-style resume variable; ignored on restore.
pub const AGENT_RESUME_ARGS: &str = "agent.resume_args";
pub const AGENT_SHUTDOWN: &str = "agent.shutdown";
const AGENT_SESSION_ID: &str = "agent.session_id";
const SESSION_VERSION: u32 = 3;
const HOST_DOMAIN: &str = "local";
const MAX_RESTORE_DIAGNOSTICS: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnInfo {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub domain: String,
    pub skip_domain_fixup: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneState {
    pub cwd: Option<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    // Legacy commands that already passed through domain fixup still need to
    // bypass it when saved again in the current format.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skip_domain_fixup: bool,
    #[serde(default)]
    pub user_vars: BTreeMap<String, String>,
}

/// Pane layout for version 2 sessions.  `Leaf` indexes into `TabState.panes`,
/// which is stored in preorder: the first/left/top subtree before the
/// second/right/bottom one, matching the order in which restore spawns panes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LayoutNode {
    Leaf(usize),
    Split {
        direction: SplitDirection,
        first_size: TerminalSize,
        second_size: TerminalSize,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabState {
    pub title: String,
    /// Version 1's single-pane form.  Version 2 writes `panes` instead; the
    /// field stays optional so old snapshots keep parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneState>,
    /// Every pane in the tab, in layout preorder.
    #[serde(default)]
    pub panes: Vec<PaneState>,
    /// Absent for a single-pane tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutNode>,
    /// Index into `panes` of the active pane.  When `zoomed` is set this is
    /// the zoomed pane, because zoom is what the viewer sees.
    #[serde(default)]
    pub active_pane: usize,
    /// Whether the active pane was zoomed when saved.
    #[serde(default, skip_serializing_if = "is_false")]
    pub zoomed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_workspace: Option<String>,
    #[serde(default)]
    pub windows: Vec<WindowState>,
}

#[derive(Clone, Debug)]
pub struct SaveSummary {
    pub path: PathBuf,
    pub windows: usize,
    pub tabs: usize,
    pub panes: usize,
    /// Panes that could not be represented because they have no local spawn
    /// command (for example ssh or tmux client panes).
    pub skipped: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RestoreSummary {
    pub windows: usize,
    pub tabs: usize,
    pub failed: usize,
    pub panes: usize,
    pub failed_panes: usize,
    pub replaced: bool,
    pub diagnostics: Vec<String>,
}

impl RestoreSummary {
    fn report(&mut self, detail: String) {
        log::warn!("restore workspace: {detail}");
        if self.diagnostics.len() < MAX_RESTORE_DIAGNOSTICS {
            let detail = detail.replace(['\r', '\n', '\x1b'], " ");
            self.diagnostics.push(detail.chars().take(512).collect());
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CloseSummary {
    pub closed: usize,
    pub remaining: usize,
}

impl SessionState {
    pub fn new() -> Self {
        Self {
            version: SESSION_VERSION,
            created_at: Utc::now().to_rfc3339(),
            active_workspace: None,
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
            domain: None,
            skip_domain_fixup: false,
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
            &[(
                AGENT_RESTORE_ARGV,
                r#"["zsh","-i","-c","claude --resume 123"]"#,
            )],
        );
        let command = command_for(&pane).unwrap();
        assert!(command.used_restore_argv);
        assert_eq!(
            argv_of(&command),
            vec!["zsh", "-i", "-c", "claude --resume 123"]
        );
        // The recorded base argv must stay the original spawn command.
        assert_eq!(
            restored_spawn_info(&pane, Some(&command)).unwrap().argv,
            ["zsh", "-i"]
        );
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
    fn empty_cwd_is_ignored() {
        let mut pane = pane_with_vars(&["sh"], &[]);
        pane.cwd = Some(String::new());
        let command = command_for(&pane).unwrap();
        assert!(command.command.get_cwd().is_none());
    }

    #[test]
    fn restore_records_an_empty_base_argv() {
        let pane_state = pane_with_vars(&[], &[(AGENT_RESTORE_ARGV, r#"["agent"]"#)]);
        let command = command_for(&pane_state).unwrap();
        let info = restored_spawn_info(&pane_state, Some(&command)).unwrap();
        assert!(info.argv.is_empty());
        assert_eq!(info.cwd.as_deref(), Some("/tmp"));
        assert!(restored_spawn_info(&pane_with_vars(&["sh"], &[]), None).is_none());
    }

    #[test]
    fn session_state_round_trips() {
        let state = SessionState {
            version: SESSION_VERSION,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            active_workspace: Some("work".to_string()),
            windows: vec![WindowState {
                workspace: "default".to_string(),
                active_tab: 0,
                tabs: vec![TabState {
                    title: "tab".to_string(),
                    pane: None,
                    panes: vec![PaneState {
                        cwd: Some("/tmp".to_string()),
                        argv: vec!["sh".to_string()],
                        env: BTreeMap::new(),
                        title: "sh".to_string(),
                        domain: Some(HOST_DOMAIN.to_string()),
                        skip_domain_fixup: false,
                        user_vars: BTreeMap::new(),
                    }],
                    layout: None,
                    active_pane: 0,
                    zoomed: false,
                }],
            }],
        };
        let encoded = serde_json::to_string(&state).unwrap();
        // Version 2 writes `panes`, not the version 1 `pane` key.
        assert!(encoded.contains("\"panes\""));
        assert!(!encoded.contains("\"pane\""));
        let decoded: SessionState = serde_json::from_str(&encoded).unwrap();
        assert_eq!(state, decoded);
    }

    #[test]
    fn layout_uses_the_documented_json_shape() {
        let layout = LayoutNode::Split {
            direction: SplitDirection::Vertical,
            first_size: TerminalSize {
                cols: 60,
                rows: 40,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            second_size: TerminalSize {
                cols: 60,
                rows: 10,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            first: Box::new(LayoutNode::Leaf(0)),
            second: Box::new(LayoutNode::Leaf(1)),
        };
        let value = serde_json::to_value(&layout).unwrap();
        assert_eq!(value["Split"]["direction"], "Vertical");
        assert_eq!(value["Split"]["first"], serde_json::json!({"Leaf": 0}));
        assert_eq!(value["Split"]["second"], serde_json::json!({"Leaf": 1}));
        assert_eq!(value["Split"]["first_size"]["cols"], 60);
    }

    #[test]
    fn version_1_snapshot_normalizes_to_a_single_pane_tab() {
        let dir = std::env::temp_dir().join(format!("wezterm-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("v1.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "created_at": "2026-01-01T00:00:00Z",
                "windows": [{
                    "workspace": "default",
                    "active_tab": 0,
                    "tabs": [{
                        "title": "old",
                        "active_tab": 0,
                        "pane": {"cwd": "/tmp", "argv": ["sh"], "title": "sh"}
                    }]
                }]
            }"#,
        )
        .unwrap();
        let state = read_json(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let tab = &state.windows[0].tabs[0];
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(tab.panes[0].argv, vec!["sh".to_string()]);
        assert!(tab.pane.is_none());
        assert!(tab.layout.is_none());
        assert_eq!(tab.active_pane, 0);
        assert!(!tab.zoomed);
    }

    fn leaf(index: usize) -> LayoutNode {
        LayoutNode::Leaf(index)
    }

    fn split(first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            direction: SplitDirection::Horizontal,
            first_size: TerminalSize {
                cols: 10,
                rows: 5,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            second_size: TerminalSize {
                cols: 20,
                rows: 5,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn pruning_collapses_a_split_with_one_remaining_side() {
        let tree = split(leaf(0), split(leaf(1), leaf(2)));
        let pruned = prune_layout(&tree, &|index| index != 1).unwrap();
        // Leaf 1's split collapses to leaf 2, which takes its place.
        match pruned {
            LayoutNode::Split { second, .. } => {
                assert_eq!(*second, leaf(2));
            }
            other => panic!("expected a split, got {:?}", other),
        }
        assert_eq!(prune_layout(&tree, &|_| false), None);
        assert_eq!(
            prune_layout(&tree, &|_| true).unwrap(),
            tree,
            "nothing pruned keeps the tree"
        );
    }

    #[test]
    fn compacting_renumbers_leaves_in_preorder() {
        let tree = split(split(leaf(0), leaf(2)), leaf(3));
        let slots = vec![
            Some(pane_with_vars(&["a"], &[])),
            None,
            Some(pane_with_vars(&["b"], &[])),
            Some(pane_with_vars(&["c"], &[])),
        ];
        let pruned = prune_layout(&tree, &|index| {
            slots.get(index).is_some_and(|s| s.is_some())
        })
        .unwrap();
        let mut mapping = HashMap::new();
        let mut panes = Vec::new();
        let compacted = compact_layout(&pruned, &slots, &mut mapping, &mut panes);
        assert_eq!(panes.len(), 3);
        assert_eq!(mapping.get(&0), Some(&0));
        assert_eq!(mapping.get(&2), Some(&1));
        assert_eq!(mapping.get(&3), Some(&2));
        match compacted {
            LayoutNode::Split { first, second, .. } => {
                assert_eq!(*second, leaf(2));
                match *first {
                    LayoutNode::Split { first, second, .. } => {
                        assert_eq!(*first, leaf(0));
                        assert_eq!(*second, leaf(1));
                    }
                    other => panic!("expected a nested split, got {:?}", other),
                }
            }
            other => panic!("expected a split, got {:?}", other),
        }
        assert_eq!(panes[0].argv, vec!["a".to_string()]);
        assert_eq!(panes[1].argv, vec!["b".to_string()]);
    }

    #[test]
    fn split_sizes_preserve_proportions_and_leave_room_for_both_panes() {
        // Same node size: exact cell count.
        assert_eq!(split_size_for(31, 10, 20), Some(SplitSize::Cells(20)));
        // A shrunken node keeps the proportion.
        assert_eq!(split_size_for(15, 10, 20), Some(SplitSize::Cells(9)));
        // Too small on either side: collapse instead of a zero-size pane.
        assert_eq!(split_size_for(2, 10, 20), None);
        assert_eq!(split_size_for(10, 0, 9), None);
        // A 3-cell node keeps one cell on each side.
        assert_eq!(split_size_for(3, 10, 20), Some(SplitSize::Cells(1)));
        assert_eq!(split_size_for(80, 1, 148), Some(SplitSize::Cells(78)));
    }
}

fn path_string(url: &url::Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path()
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| {
            percent_encoding::percent_decode_str(url.path())
                .decode_utf8()
                .ok()
                .filter(|path| !path.is_empty())
                .map(|path| path.into_owned())
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

fn normalize_state(state: &mut SessionState) {
    for window in &mut state.windows {
        for tab in &mut window.tabs {
            if tab.panes.is_empty() {
                // Version 1: exactly one pane in `pane`.
                if let Some(pane) = tab.pane.take() {
                    tab.panes.push(pane);
                }
                tab.layout = None;
                tab.active_pane = 0;
                tab.zoomed = false;
            }
            if state.version < SESSION_VERSION {
                for pane in &mut tab.panes {
                    pane.domain = Some(HOST_DOMAIN.to_string());
                    pane.skip_domain_fixup = !pane.argv.is_empty();
                }
            }
        }
    }
}

fn read_json(path: &Path) -> Result<SessionState> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut state: SessionState = serde_json::from_slice(&data)
        .with_context(|| format!("parsing workspace {}", path.display()))?;
    if !(1..=SESSION_VERSION).contains(&state.version) {
        return Err(anyhow!(
            "unsupported workspace version {} in {}",
            state.version,
            path.display()
        ));
    }
    normalize_state(&mut state);
    Ok(state)
}

fn truthy(value: Option<&String>) -> bool {
    matches!(value.map(|v| v.as_str()), Some("1" | "true" | "yes"))
}

fn shutdown_identity(pane: &Arc<dyn Pane>) -> Option<(Option<String>, Option<String>)> {
    if pane.is_dead() {
        return None;
    }
    let mut vars = pane.copy_user_vars();
    truthy(vars.get(AGENT_SHUTDOWN)).then(|| {
        let session = vars.remove(AGENT_SESSION_ID);
        let command = if session.is_none() {
            vars.remove(AGENT_RESTORE_ARGV)
        } else {
            None
        };
        (session, command)
    })
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
    used_restore_argv: bool,
}

fn command_for(pane: &PaneState) -> Option<RestoreCommand> {
    if pane.user_vars.contains_key(AGENT_RESUME_ARGS) {
        // Both vars are present on snapshots written during the transition:
        // restore_argv wins, so the obsolete one is just debug noise. Warn
        // only when it is the only metadata, because then the session cannot
        // resume.
        if pane.user_vars.contains_key(AGENT_RESTORE_ARGV) {
            log::debug!(
                "restore workspace: ignoring obsolete {AGENT_RESUME_ARGS}: argv={:?}",
                pane.argv
            );
        } else {
            log::warn!(
                "restore workspace: ignoring obsolete {AGENT_RESUME_ARGS} (use {AGENT_RESTORE_ARGV}): argv={:?}",
                pane.argv
            );
        }
    }
    let replacement = restore_argv(&pane.user_vars);
    let used_restore_argv = replacement.is_some();
    let argv = replacement.unwrap_or_else(|| pane.argv.clone());
    let mut command = match argv.split_first() {
        Some((program, args)) => {
            let mut command = portable_pty::CommandBuilder::new(program);
            command.args(args);
            command
        }
        None if !pane.env.is_empty() => portable_pty::CommandBuilder::new_default_prog(),
        None => return None,
    };
    if let Some(cwd) = pane.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        command.cwd(cwd);
    }
    for (key, value) in &pane.env {
        command.env(key, value);
    }
    Some(RestoreCommand {
        command,
        used_restore_argv,
    })
}

/// Spawn info to record after restoring a pane.  When the pane was started
/// from `agent.restore_argv`, the original spawn argv is recorded even when it
/// is empty (the default-shell case): recording the replacement command would
/// make the next save restore the old agent session again.
fn restored_spawn_info(pane: &PaneState, command: Option<&RestoreCommand>) -> Option<SpawnInfo> {
    command
        .filter(|cmd| cmd.used_restore_argv)
        .map(|_| SpawnInfo {
            argv: pane.argv.clone(),
            cwd: pane.cwd.clone(),
            env: pane.env.clone(),
            domain: pane.domain.as_deref().unwrap_or(HOST_DOMAIN).to_string(),
            skip_domain_fixup: pane.skip_domain_fixup,
        })
}

fn spawn_domain(pane: &PaneState) -> SpawnTabDomain {
    SpawnTabDomain::DomainName(pane.domain.as_deref().unwrap_or(HOST_DOMAIN).to_string())
}

fn skip_domain_fixup(pane: &PaneState, command: Option<&RestoreCommand>) -> bool {
    pane.skip_domain_fixup && command.is_some_and(|cmd| !cmd.used_restore_argv)
}

/// Serializable state for one pane, or `None` when the pane has no local
/// spawn command (ssh/tmux); such panes are pruned instead of being silently
/// replaced by the default shell.
fn pane_state(pane: &Arc<dyn Pane>) -> Option<PaneState> {
    let spawn = pane.get_spawn_info()?;
    // A named domain can map its working directory just like its argv.
    // An OSC 7 path comes from inside the domain; a host process path may
    // already have been transformed and should not be passed through again.
    let cwd = if spawn.domain == HOST_DOMAIN {
        pane.get_current_working_dir(CachePolicy::AllowStale)
    } else {
        pane.downcast_ref::<LocalPane>()
            .and_then(LocalPane::reported_working_dir)
    }
    .as_ref()
    .and_then(path_string)
    .or_else(|| spawn.cwd.clone());
    let user_vars = pane
        .copy_user_vars()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    Some(PaneState {
        cwd,
        argv: spawn.argv.clone(),
        env: spawn.env.clone(),
        title: pane.get_title(),
        domain: Some(spawn.domain),
        skip_domain_fixup: spawn.skip_domain_fixup,
        user_vars,
    })
}

/// Convert a tab's pane tree into a serializable layout.  Leaf indices are the
/// tab's topological pane indices (the same ones `iter_panes_ignoring_zoom`
/// reports), so active/zoom indices line up before pruning.
fn layout_to_state(
    node: &TabLayoutNode,
    index_of: &HashMap<PaneId, usize>,
    panes: &mut Vec<Option<PaneState>>,
    skipped: &mut usize,
) -> LayoutNode {
    match node {
        TabLayoutNode::Leaf { pane } => {
            let index = *index_of
                .get(&pane.pane_id())
                .expect("a layout pane belongs to its tab");
            let state = pane_state(pane);
            if state.is_none() {
                *skipped += 1;
                log::warn!(
                    "save workspace: skipping pane {}; it has no local spawn command",
                    pane.pane_id()
                );
            }
            if panes.len() <= index {
                panes.resize(index + 1, None);
            }
            panes[index] = state;
            LayoutNode::Leaf(index)
        }
        TabLayoutNode::Split {
            direction,
            first_size,
            second_size,
            first_node,
            second_node,
        } => {
            let first = layout_to_state(first_node, index_of, panes, skipped);
            let second = layout_to_state(second_node, index_of, panes, skipped);
            LayoutNode::Split {
                direction: *direction,
                first_size: *first_size,
                second_size: *second_size,
                first: Box::new(first),
                second: Box::new(second),
            }
        }
    }
}

/// Drop leaves whose panes cannot be restored.  A split with one remaining
/// side collapses to that side (which then fills the whole node area); a split
/// with none is dropped.  This keeps every leaf in the saved file local.
fn prune_layout<F>(node: &LayoutNode, keep: &F) -> Option<LayoutNode>
where
    F: Fn(usize) -> bool,
{
    match node {
        LayoutNode::Leaf(index) => keep(*index).then_some(LayoutNode::Leaf(*index)),
        LayoutNode::Split {
            direction,
            first_size,
            second_size,
            first,
            second,
        } => match (prune_layout(first, keep), prune_layout(second, keep)) {
            (Some(first), Some(second)) => Some(LayoutNode::Split {
                direction: *direction,
                first_size: *first_size,
                second_size: *second_size,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(node), None) | (None, Some(node)) => Some(node),
            (None, None) => None,
        },
    }
}

/// Renumber surviving leaves to compact preorder indices and copy their pane
/// states into `panes`.  `mapping` maps the old topological index to the new
/// one for active/zoom lookup.
fn compact_layout(
    node: &LayoutNode,
    slots: &[Option<PaneState>],
    mapping: &mut HashMap<usize, usize>,
    panes: &mut Vec<PaneState>,
) -> LayoutNode {
    match node {
        LayoutNode::Leaf(index) => {
            let state = slots
                .get(*index)
                .and_then(|slot| slot.clone())
                .expect("a pruned leaf keeps its pane state");
            let new_index = panes.len();
            mapping.insert(*index, new_index);
            panes.push(state);
            LayoutNode::Leaf(new_index)
        }
        LayoutNode::Split {
            direction,
            first_size,
            second_size,
            first,
            second,
        } => LayoutNode::Split {
            direction: *direction,
            first_size: *first_size,
            second_size: *second_size,
            first: Box::new(compact_layout(first, slots, mapping, panes)),
            second: Box::new(compact_layout(second, slots, mapping, panes)),
        },
    }
}

fn first_leaf(node: &LayoutNode) -> usize {
    match node {
        LayoutNode::Leaf(index) => *index,
        LayoutNode::Split { first, .. } => first_leaf(first),
    }
}

fn layout_minimum(node: &LayoutNode, axis: SplitDirection) -> usize {
    match node {
        LayoutNode::Leaf(_) => 1,
        LayoutNode::Split {
            direction,
            first,
            second,
            ..
        } => {
            let first = layout_minimum(first, axis);
            let second = layout_minimum(second, axis);
            if *direction == axis {
                first.saturating_add(second).saturating_add(1)
            } else {
                first.max(second)
            }
        }
    }
}

/// Scale the saved second pane into the available cells and leave a separator
/// and at least one cell for the first pane. A cell request avoids the rounding
/// of the percent-based splitter for narrow or heavily asymmetric splits.
fn split_size_for(axis_total: usize, first: usize, second: usize) -> Option<SplitSize> {
    if axis_total < 3 || first == 0 || second == 0 {
        return None;
    }
    let saved_pane_total = first as u128 + second as u128;
    let available = (axis_total - 1) as u128;
    let second = (second as u128 * available + saved_pane_total / 2) / saved_pane_total;
    Some(SplitSize::Cells((second as usize).clamp(1, axis_total - 2)))
}

/// Rebuild splits top-down. If the second branch cannot be placed, the
/// first branch can still subdivide the entire area occupied by the anchor.
fn build_layout_node<'a>(
    mux: &'a Mux,
    panes: &'a [PaneState],
    node: &'a LayoutNode,
    anchor: Arc<dyn Pane>,
    leaf_pane_ids: &'a mut [Option<PaneId>],
    errors: &'a mut Vec<String>,
) -> Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let LayoutNode::Split {
            direction,
            first_size,
            second_size,
            first,
            second,
        } = node
        else {
            return;
        };
        let dims = anchor.get_dimensions();
        let (axis_total, first_axis, second_axis) = match direction {
            SplitDirection::Horizontal => (dims.cols, first_size.cols, second_size.cols),
            SplitDirection::Vertical => (dims.viewport_rows, first_size.rows, second_size.rows),
        };
        let second_leaf = first_leaf(second);
        let size = split_size_for(axis_total, first_axis, second_axis).map(|size| {
            let min_second = layout_minimum(second, *direction);
            let max_second = (axis_total - 1).saturating_sub(layout_minimum(first, *direction));
            match size {
                SplitSize::Cells(cells) if min_second <= max_second => {
                    SplitSize::Cells(cells.clamp(min_second, max_second))
                }
                _ => size,
            }
        });
        let outcome = match (size, panes.get(second_leaf)) {
            (Some(size), Some(second_state)) if leaf_pane_ids[second_leaf].is_none() => {
                let command = command_for(second_state);
                let skip_fixup = skip_domain_fixup(second_state, command.as_ref());
                let spawn_info = restored_spawn_info(second_state, command.as_ref());
                mux.split_pane(
                    anchor.pane_id(),
                    SplitRequest {
                        direction: *direction,
                        target_is_second: true,
                        top_level: false,
                        size,
                    },
                    SplitSource::Restore {
                        command: command.map(|cmd| cmd.command),
                        command_dir: nonempty_cwd(second_state),
                        skip_domain_fixup: skip_fixup,
                    },
                    spawn_domain(second_state),
                )
                .await
                .map(|(pane, _)| {
                    if spawn_info.is_some() {
                        pane.set_spawn_info(spawn_info);
                    }
                    pane
                })
            }
            (None, _) => Err(anyhow!(
                "{direction:?} split does not fit in {axis_total} cells"
            )),
            _ => Err(anyhow!("invalid or duplicate split leaf {second_leaf}")),
        };
        let second_pane = match outcome {
            Ok(new_pane) => {
                leaf_pane_ids[second_leaf] = Some(new_pane.pane_id());
                Some(new_pane)
            }
            Err(err) => {
                errors.push(format!("pane {second_leaf}: {err:#}"));
                None
            }
        };
        build_layout_node(mux, panes, first, anchor, leaf_pane_ids, errors).await;
        if let Some(second_pane) = second_pane {
            build_layout_node(mux, panes, second, second_pane, leaf_pane_ids, errors).await;
        }
    })
}

fn nonempty_cwd(pane: &PaneState) -> Option<String> {
    pane.cwd.clone().filter(|cwd| !cwd.is_empty())
}

impl Mux {
    pub async fn save_workspace(&self, path: &Path) -> Result<SaveSummary> {
        let _operation = self.session_operation.lock().await;
        let mut state = SessionState::new();
        state.active_workspace = Some(self.active_workspace());
        let mut window_ids = self.iter_windows();
        window_ids.sort();
        let mut skipped = 0usize;

        for window_id in window_ids {
            let window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window {window_id} disappeared while saving"))?;
            let active_tab_idx = window.get_active_tab_idx();
            let mut tabs = Vec::new();
            let mut saved_active_tab = None;
            for (tab_idx, tab) in window.iter_tabs().enumerate() {
                let title = tab.get_title();
                let Some(root) = tab.layout_node() else {
                    continue;
                };
                let Some(active_pane) = tab.get_active_pane() else {
                    continue;
                };
                let zoomed_pane = tab.get_zoomed_pane();
                // The topological index is what active/zoom positions refer
                // to; the saved preorder indices are recomputed after pruning.
                let positioned = tab.iter_panes_ignoring_zoom();
                let index_of: HashMap<PaneId, usize> = positioned
                    .iter()
                    .map(|pane| (pane.pane.pane_id(), pane.index))
                    .collect();
                let mut pane_slots: Vec<Option<PaneState>> = Vec::new();
                let raw = layout_to_state(&root, &index_of, &mut pane_slots, &mut skipped);
                let Some(pruned) = prune_layout(&raw, &|index| {
                    pane_slots.get(index).is_some_and(|slot| slot.is_some())
                }) else {
                    log::warn!(
                        "save workspace: skipping tab {title:?}; no pane has a local spawn command"
                    );
                    continue;
                };
                let mut mapping = HashMap::new();
                let mut panes = Vec::new();
                let layout = compact_layout(&pruned, &pane_slots, &mut mapping, &mut panes);
                let layout = match layout {
                    LayoutNode::Leaf(_) => None,
                    other => Some(other),
                };
                let zoomed_index = zoomed_pane
                    .as_ref()
                    .and_then(|pane| index_of.get(&pane.pane_id()).copied());
                let active_index =
                    zoomed_index.or_else(|| index_of.get(&active_pane.pane_id()).copied());
                let (active_pane, zoomed) = match zoomed_index.and_then(|index| mapping.get(&index))
                {
                    // Zoom shows the zoomed pane; make it the active pane.
                    Some(index) => (*index, true),
                    None => (
                        active_index
                            .and_then(|index| mapping.get(&index))
                            .copied()
                            .unwrap_or(0),
                        false,
                    ),
                };
                if tab_idx == active_tab_idx {
                    saved_active_tab = Some(tabs.len());
                }
                tabs.push(TabState {
                    title,
                    pane: None,
                    panes,
                    layout,
                    active_pane,
                    zoomed,
                });
            }
            if tabs.is_empty() {
                continue;
            }
            state.windows.push(WindowState {
                workspace: window.get_workspace().to_string(),
                // Pruning can drop or reorder tabs; prefer the saved position
                // of the active tab, then clamp into the surviving range.
                active_tab: saved_active_tab.unwrap_or_else(|| active_tab_idx.min(tabs.len() - 1)),
                tabs,
            });
        }

        if skipped > 0 {
            log::warn!(
                "save workspace: skipped {skipped} pane(s) without a local spawn command (ssh/tmux)"
            );
        }
        if skipped > 0 && state.windows.is_empty() {
            anyhow::bail!(
                "no panes could be saved ({skipped} unsupported); keeping the prior snapshot"
            );
        }
        let pane_count = state
            .windows
            .iter()
            .flat_map(|window| &window.tabs)
            .map(|tab| tab.panes.len())
            .sum();
        write_json(path, &state)?;
        Ok(SaveSummary {
            path: path.to_path_buf(),
            windows: state.windows.len(),
            tabs: state.windows.iter().map(|window| window.tabs.len()).sum(),
            panes: pane_count,
            skipped,
        })
    }

    pub async fn restore_workspace(
        &self,
        path: &Path,
        target_window: Option<crate::window::WindowId>,
        replace: bool,
    ) -> Result<RestoreSummary> {
        let _operation = self.session_operation.lock().await;
        let state = read_json(path)?;
        let target_window = match target_window {
            Some(id) => Some(id),
            None => match self.iter_windows().as_slice() {
                [id] => Some(*id),
                windows if replace && windows.len() > 1 => anyhow::bail!(
                    "restore-workspace --replace requires --window-id when more than one window exists"
                ),
                _ => None,
            },
        };
        let target_before = target_window
            .map(|id| {
                let window = self
                    .get_window(id)
                    .ok_or_else(|| anyhow!("no such window {id}"))?;
                let tabs = window
                    .iter_tabs()
                    .map(|tab| tab.tab_id())
                    .collect::<Vec<_>>();
                let active = window.get_active_tab().map(|tab| tab.tab_id());
                Ok::<_, anyhow::Error>((tabs, active))
            })
            .transpose()?;
        let mut summary = RestoreSummary::default();
        let mut restored_workspaces = Vec::new();
        let mut replacement_workspace = None;
        for (window_index, saved_window) in state.windows.iter().enumerate() {
            let mut current_window = if window_index == 0 {
                target_window
            } else {
                None
            };
            let replacing_here = replace && current_window.is_some();
            let workspace = if saved_window.workspace.is_empty() {
                crate::DEFAULT_WORKSPACE
            } else {
                &saved_window.workspace
            };
            let mut restored_tabs = Vec::new();
            let mut window_panes = 0;
            let mut complete = !saved_window.tabs.is_empty();
            if saved_window.tabs.is_empty() {
                summary.failed += 1;
                summary.report(format!("window {window_index} has no tabs"));
            }
            for (tab_index, saved_tab) in saved_window.tabs.iter().enumerate() {
                let root_leaf = saved_tab.layout.as_ref().map_or(0, first_leaf);
                let Some(root_state) = saved_tab.panes.get(root_leaf) else {
                    summary.failed += 1;
                    summary.failed_panes += saved_tab.panes.len().max(1);
                    complete = false;
                    summary.report(format!(
                        "window {window_index} tab {tab_index}: missing root pane {root_leaf}"
                    ));
                    continue;
                };
                let command = command_for(root_state);
                let skip_fixup = skip_domain_fixup(root_state, command.as_ref());
                let spawn_info = restored_spawn_info(root_state, command.as_ref());
                let size = config::configuration().initial_size(0, None);
                let result = self
                    .spawn_tab_for_restore(
                        current_window,
                        spawn_domain(root_state),
                        command.map(|cmd| cmd.command),
                        nonempty_cwd(root_state),
                        size,
                        workspace.to_string(),
                        skip_fixup,
                    )
                    .await;
                let (tab, pane, window_id) = match result {
                    Ok(result) => result,
                    Err(err) => {
                        summary.failed += 1;
                        summary.failed_panes += saved_tab.panes.len();
                        complete = false;
                        summary.report(format!(
                            "window {window_index} tab {tab_index}: root pane {root_leaf}: {err:#}"
                        ));
                        continue;
                    }
                };
                tab.set_title(&saved_tab.title);
                if spawn_info.is_some() {
                    pane.set_spawn_info(spawn_info);
                }
                let mut leaf_pane_ids = vec![None; saved_tab.panes.len()];
                leaf_pane_ids[root_leaf] = Some(pane.pane_id());
                let mut split_errors = Vec::new();
                if let Some(layout) = &saved_tab.layout {
                    build_layout_node(
                        self,
                        &saved_tab.panes,
                        layout,
                        pane,
                        &mut leaf_pane_ids,
                        &mut split_errors,
                    )
                    .await;
                }
                let pane_count = leaf_pane_ids.iter().flatten().count();
                let missing = saved_tab.panes.len() - pane_count;
                if missing > 0 || !split_errors.is_empty() {
                    summary.failed_panes += missing;
                    complete = false;
                    if split_errors.is_empty() {
                        summary.report(format!("window {window_index} tab {tab_index}: layout omitted {missing} pane(s)"));
                    }
                    for error in split_errors {
                        summary.report(format!("window {window_index} tab {tab_index}: {error}"));
                    }
                    if missing == 0 {
                        summary.failed += 1;
                    }
                }
                let target_pane = leaf_pane_ids
                    .get(saved_tab.active_pane)
                    .copied()
                    .flatten()
                    .or_else(|| leaf_pane_ids.iter().copied().flatten().next());
                if let Some(position) = tab
                    .iter_panes_ignoring_zoom()
                    .iter()
                    .position(|positioned| Some(positioned.pane.pane_id()) == target_pane)
                {
                    tab.set_active_idx(position);
                }
                if saved_tab.zoomed && tab.get_zoomed_pane().is_none() {
                    tab.toggle_zoom();
                }
                current_window = Some(window_id);
                restored_tabs.push((tab_index, tab.tab_id()));
                window_panes += pane_count;
            }

            let Some(window_id) = current_window else {
                continue;
            };
            if replacing_here {
                let (old_tabs, old_active) = target_before.as_ref().unwrap();
                if restored_tabs
                    .iter()
                    .any(|(_, tab_id)| self.window_containing_tab(*tab_id) != Some(window_id))
                {
                    complete = false;
                    summary.failed += 1;
                    summary.report(format!(
                        "a restored tab left window {window_id} before replacement"
                    ));
                }
                if !complete {
                    summary.failed_panes += window_panes;
                    for (_, tab_id) in restored_tabs {
                        if self.window_containing_tab(tab_id) == Some(window_id) {
                            self.remove_tab(tab_id);
                        }
                    }
                    if let (Some(mut window), Some(tab_id)) =
                        (self.get_window_mut(window_id), old_active)
                    {
                        if let Some(index) = window.get_tab_idx_for_id(*tab_id) {
                            window.remember_and_set_active_tab_idx(index);
                        }
                    }
                    summary.report(format!("kept original tabs in window {window_id} because its restore was incomplete"));
                    continue;
                }
                // Tabs may have been moved while a domain callback was pending.
                // Replace only tabs still belonging to the original window.
                for &tab_id in old_tabs {
                    if self.window_containing_tab(tab_id) == Some(window_id) {
                        self.remove_tab(tab_id);
                    }
                }
                if let Some(mut window) = self.get_window_mut(window_id) {
                    window.set_workspace(workspace);
                }
                replacement_workspace = Some(workspace);
                summary.replaced = true;
            }
            if let Some((_, tab_id)) = restored_tabs
                .iter()
                .find(|(index, _)| *index == saved_window.active_tab)
                .or_else(|| restored_tabs.last())
            {
                if let Some(mut window) = self.get_window_mut(window_id) {
                    if let Some(index) = window.get_tab_idx_for_id(*tab_id) {
                        window.remember_and_set_active_tab_idx(index);
                    }
                    restored_workspaces.push(window.get_workspace().to_string());
                }
                summary.windows += 1;
                summary.tabs += restored_tabs.len();
                summary.panes += window_panes;
            }
        }

        self.recompute_pane_count();
        if replace && (target_window.is_none() || summary.replaced) {
            let activate = state
                .active_workspace
                .as_deref()
                .filter(|workspace| restored_workspaces.iter().any(|name| name == *workspace))
                .or(replacement_workspace);
            if let Some(workspace) = activate {
                self.set_active_workspace(workspace);
            }
        }
        self.prune_dead_windows();
        Ok(summary)
    }

    pub async fn close_agent_sessions(&self, timeout: Duration) -> Result<CloseSummary> {
        let _operation = self.session_operation.lock().await;
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
                    if let Some(identity) = shutdown_identity(&positioned.pane) {
                        targets.push((positioned.pane.pane_id(), identity));
                    }
                }
            }
        }
        let deadline = Instant::now() + timeout;
        let targeted = targets.len();
        // Recheck identity immediately before each input. An Esc or Ctrl+C
        // can exit an agent and expose its shell, or another session can take
        // over the same pane before the next step.
        for (input, delay) in [(b"\x1b".as_slice(), 200), (b"\x03", 250), (b"/exit\r", 0)] {
            targets.retain(|(pane_id, identity)| {
                self.get_pane(*pane_id).is_some_and(|pane| {
                    if shutdown_identity(&pane).as_ref() != Some(identity) {
                        return false;
                    }
                    let mut writer = pane.writer();
                    if let Err(err) = writer.write_all(input).and_then(|()| writer.flush()) {
                        log::warn!("close agent sessions: pane {pane_id}: {err:#}");
                    }
                    true
                })
            });
            if targets.is_empty() {
                break;
            }
            if delay > 0 {
                smol::Timer::after(
                    Duration::from_millis(delay)
                        .min(deadline.saturating_duration_since(Instant::now())),
                )
                .await;
            }
        }
        loop {
            targets.retain(|(pane_id, identity)| {
                self.get_pane(*pane_id)
                    .and_then(|pane| shutdown_identity(&pane))
                    .as_ref()
                    == Some(identity)
            });
            let wait = deadline.saturating_duration_since(Instant::now());
            if targets.is_empty() || wait.is_zero() {
                return Ok(CloseSummary {
                    closed: targeted - targets.len(),
                    remaining: targets.len(),
                });
            }
            smol::Timer::after(wait.min(Duration::from_millis(100))).await;
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
