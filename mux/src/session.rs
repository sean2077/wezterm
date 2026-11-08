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
//! Session version 2 stores every pane of a tab together with the split tree
//! (direction, per-child sizes, preorder pane list); version 1 snapshots are
//! still read and are treated as single-pane tabs.  Restore rebuilds splits
//! top-down through the normal mux split path, so no additional PDU or codec
//! change is involved.

use crate::domain::SplitSource;
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
            version: 2,
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
    fn empty_cwd_is_ignored() {
        let mut pane = pane_with_vars(&["sh"], &[]);
        pane.cwd = Some(String::new());
        let command = command_for(&pane).unwrap();
        assert!(command.command.get_cwd().is_none());
    }

    #[test]
    fn restore_records_an_empty_base_argv() {
        let info = restored_spawn_info(true, Vec::new(), Some("/tmp".to_string())).unwrap();
        assert!(info.argv.is_empty());
        assert_eq!(info.cwd.as_deref(), Some("/tmp"));
        assert!(restored_spawn_info(false, vec!["sh".to_string()], None).is_none());
    }

    #[test]
    fn session_state_round_trips() {
        let state = SessionState {
            version: 2,
            created_at: "2026-01-01T00:00:00Z".to_string(),
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
    fn split_sizes_prefer_exact_cells_and_fall_back_to_percent() {
        // Same node size: exact cell count.
        assert_eq!(split_size_for(31, 10, 20), Some(SplitSize::Cells(20)));
        // A shrunken node keeps the proportion.
        assert_eq!(split_size_for(15, 10, 20), Some(SplitSize::Percent(65)));
        // Too small on either side: collapse instead of a zero-size pane.
        assert_eq!(split_size_for(2, 10, 20), None);
        assert_eq!(split_size_for(10, 0, 9), None);
        // A 3-cell node keeps one cell on each side.
        assert_eq!(split_size_for(3, 10, 20), Some(SplitSize::Percent(65)));
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
            } else if tab.layout.is_none() && tab.panes.len() > 1 {
                // A layout-less multi-pane tab cannot describe its
                // arrangement; keep the first pane rather than silently
                // opening every pane on top of each other.
                log::warn!(
                    "restore workspace: tab {:?} has {} panes but no layout; keeping the first",
                    tab.title,
                    tab.panes.len()
                );
                tab.panes.truncate(1);
                tab.active_pane = 0;
                tab.zoomed = false;
            }
        }
    }
}

fn read_json(path: &Path) -> Result<SessionState> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut state: SessionState = serde_json::from_slice(&data)
        .with_context(|| format!("parsing workspace {}", path.display()))?;
    if state.version != 1 && state.version != 2 {
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
    if argv.is_empty() {
        return None;
    }
    let mut command = portable_pty::CommandBuilder::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    if let Some(cwd) = pane.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
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

/// Spawn info to record after restoring a pane.  When the pane was started
/// from `agent.restore_argv`, the original spawn argv is recorded even when it
/// is empty (the default-shell case): recording the replacement command would
/// make the next save restore the old agent session again.
fn restored_spawn_info(
    used_restore_argv: bool,
    base_argv: Vec<String>,
    cwd: Option<String>,
) -> Option<SpawnInfo> {
    used_restore_argv.then(|| SpawnInfo {
        argv: base_argv,
        cwd,
        env: BTreeMap::new(),
    })
}

/// Serializable state for one pane, or `None` when the pane has no local
/// spawn command (ssh/tmux); such panes are pruned instead of being silently
/// replaced by the default shell.
fn pane_state(pane: &Arc<dyn Pane>) -> Option<PaneState> {
    let spawn = pane.get_spawn_info()?;
    let cwd = pane
        .get_current_working_dir(CachePolicy::AllowStale)
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

/// Size request that rebuilds a split inside a node of `axis_total` cells.
/// Exact cells are used when the node still has its saved size; otherwise the
/// saved proportion is used.  `None` means the node is too small to split,
/// in which case restore collapses the split instead of producing a zero-size
/// pane.
fn split_size_for(axis_total: usize, first: usize, second: usize) -> Option<SplitSize> {
    if axis_total < 3 || first == 0 || second == 0 {
        return None;
    }
    let saved_total = first + second + 1;
    if saved_total == axis_total {
        return Some(SplitSize::Cells(second));
    }
    let percent = ((second as f64) * 100.0 / (saved_total as f64)).round() as i64;
    let percent = percent.clamp(1, 99) as usize;
    // Match compute_split_size's integer arithmetic: the new pane targets
    // `axis_total * percent / 100` cells (at least one); the first side must
    // still get at least one cell plus the separator.
    let target = (axis_total * percent / 100).max(1);
    if axis_total.saturating_sub(target + 1) < 1 {
        return None;
    }
    Some(SplitSize::Percent(percent as u8))
}

/// Rebuild the splits below `anchor` top-down.  The anchor currently occupies
/// the whole node area; the second subtree's first leaf is spawned into the
/// new pane, then both subtrees recurse.  A failed split leaves the anchor in
/// place and skips that subtree: the rest of the tab keeps working.
fn build_layout_node<'a>(
    mux: &'a Mux,
    panes: &'a [PaneState],
    node: &'a LayoutNode,
    anchor: Arc<dyn Pane>,
    leaf_pane_ids: &'a mut Vec<Option<PaneId>>,
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
            SplitDirection::Horizontal => (
                dims.cols,
                first_size.cols as usize,
                second_size.cols as usize,
            ),
            SplitDirection::Vertical => (
                dims.viewport_rows,
                first_size.rows as usize,
                second_size.rows as usize,
            ),
        };
        let Some(size) = split_size_for(axis_total, first_axis, second_axis) else {
            log::warn!(
                "restore workspace: collapsing a {direction:?} split that does not fit (axis={axis_total}, first={first_axis}, second={second_axis})"
            );
            return;
        };
        let second_leaf = first_leaf(second);
        let Some(second_state) = panes.get(second_leaf) else {
            log::warn!("restore workspace: split leaf {second_leaf} is out of range");
            return;
        };
        let (command, used_restore_argv, base_argv) = match command_for(second_state) {
            Some(restore) => (
                Some(restore.command),
                restore.used_restore_argv,
                restore.base_argv,
            ),
            None => (None, false, Vec::new()),
        };
        let outcome = mux
            .split_pane(
                anchor.pane_id(),
                SplitRequest {
                    direction: *direction,
                    target_is_second: true,
                    top_level: false,
                    size,
                },
                SplitSource::Spawn {
                    command,
                    command_dir: second_state.cwd.clone(),
                },
                SpawnTabDomain::DefaultDomain,
            )
            .await;
        match outcome {
            Ok((new_pane, _size)) => {
                if let Some(info) =
                    restored_spawn_info(used_restore_argv, base_argv, second_state.cwd.clone())
                {
                    new_pane.set_spawn_info(Some(info));
                }
                if let Some(slot) = leaf_pane_ids.get_mut(second_leaf) {
                    *slot = Some(new_pane.pane_id());
                }
                build_layout_node(mux, panes, first, Arc::clone(&anchor), &mut *leaf_pane_ids)
                    .await;
                build_layout_node(mux, panes, second, new_pane, &mut *leaf_pane_ids).await;
            }
            Err(err) => {
                log::warn!(
                    "restore workspace: could not rebuild a {direction:?} split (first={first_axis}, second={second_axis}, node={axis_total}); keeping the anchor pane: {err:#}"
                );
            }
        }
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
                if saved_tab.panes.is_empty() {
                    log::error!(
                        "restore workspace: window {window_index} tab {tab_index} ({:?}) has no panes",
                        saved_tab.title
                    );
                    summary.failed += 1;
                    continue;
                }
                // The tab's initial pane is the first leaf of the layout
                // (left/top-most).  Empty argv means the default shell.
                let root_leaf = match &saved_tab.layout {
                    Some(layout) => first_leaf(layout),
                    None => 0,
                };
                let Some(root_state) = saved_tab.panes.get(root_leaf) else {
                    log::error!(
                        "restore workspace: window {window_index} tab {tab_index} ({:?}) references missing pane {root_leaf}",
                        saved_tab.title
                    );
                    summary.failed += 1;
                    continue;
                };
                let (command, used_restore_argv, base_argv) = match command_for(root_state) {
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
                        root_state.cwd.clone(),
                        size,
                        None,
                        workspace,
                        None,
                    )
                    .await;
                match result {
                    Ok((tab, pane, window_id)) => {
                        tab.set_title(&saved_tab.title);
                        // Remember the pane's original spawn command, not the
                        // replacement command, so repeated restore cycles
                        // never accumulate arguments.  An empty base argv is
                        // recorded as-is: it means "default shell", and
                        // recording the replacement command would resurrect
                        // the old session on the next save.
                        if let Some(info) = restored_spawn_info(
                            used_restore_argv,
                            base_argv,
                            root_state.cwd.clone(),
                        ) {
                            pane.set_spawn_info(Some(info));
                        }
                        let mut leaf_pane_ids = vec![None; saved_tab.panes.len()];
                        leaf_pane_ids[root_leaf] = Some(pane.pane_id());
                        if let Some(layout) = &saved_tab.layout {
                            build_layout_node(
                                self,
                                &saved_tab.panes,
                                layout,
                                Arc::clone(&pane),
                                &mut leaf_pane_ids,
                            )
                            .await;
                        }
                        // A failed split collapses its subtree, so resolve
                        // the saved active leaf through the panes that were
                        // actually created.
                        let target = leaf_pane_ids
                            .get(saved_tab.active_pane)
                            .copied()
                            .flatten()
                            .or_else(|| leaf_pane_ids.iter().copied().flatten().next());
                        if let Some(pane_id) = target {
                            if let Some(position) = tab
                                .iter_panes_ignoring_zoom()
                                .iter()
                                .position(|positioned| positioned.pane.pane_id() == pane_id)
                            {
                                tab.set_active_idx(position);
                            }
                        }
                        if saved_tab.zoomed && tab.get_zoomed_pane().is_none() {
                            tab.toggle_zoom();
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
                let idx = window.iter_tabs().position(|tab| tab.tab_id() == tab_id);
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
