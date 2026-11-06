use clap::Parser;
use codec::RestoreWorkspace;
use mux::window::WindowId;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct RestoreWorkspaceCommand {
    /// Read the workspace snapshot from this path.
    #[arg(long)]
    path: PathBuf,

    /// Restore the first saved window into this existing window.
    /// Defaults to the only window when there is exactly one.
    #[arg(long)]
    window_id: Option<WindowId>,

    /// Remove the target window's existing tabs after the restored tabs are
    /// spawned. Use this after launching a fresh GUI with one startup pane.
    #[arg(long)]
    replace: bool,

    /// One-time migration: read the legacy Claude tab pending.json format.
    /// The conversion is deliberately confined to this CLI adapter.
    #[arg(long)]
    legacy_claude_pending: bool,
}

impl RestoreWorkspaceCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let mut temporary: Option<PathBuf> = None;
        let path = if self.legacy_claude_pending {
            let converted = convert_legacy_claude_pending(&self.path)?;
            temporary = Some(converted.clone());
            converted
        } else {
            self.path.clone()
        };
        let result = client
            .restore_workspace(RestoreWorkspace {
                path: path.to_string_lossy().to_string(),
                window_id: self.window_id,
                replace: self.replace,
            })
            .await;
        if let Some(temporary) = temporary {
            let _ = std::fs::remove_file(temporary);
        }
        let response = result?;
        println!(
            "WEZTERM-WORKSPACE: restored windows={} tabs={} failed={}",
            response.windows, response.tabs, response.failed
        );
        if response.failed > 0 {
            anyhow::bail!("{} tab(s) failed to restore", response.failed);
        }
        Ok(())
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_tail(path: &str) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let take = length.min(8 * 1024 * 1024);
    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::End(-(take as i64))).is_err() {
        return String::new();
    }
    let mut buffer = Vec::with_capacity(take as usize);
    let _ = file.read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).to_string()
}

fn infer_from_transcript(path: &str) -> (String, String) {
    let text = read_tail(path);
    let mut model = String::new();
    let mut fallback_model = String::new();
    let mut effort = String::new();
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if model.is_empty() {
            if let Some(candidate) = value
                .pointer("/params/model")
                .or_else(|| value.pointer("/message/model"))
                .and_then(Value::as_str)
            {
                let lower = candidate.to_lowercase();
                if lower.contains("deepseek") || lower.contains("glm") {
                    model = candidate.to_string();
                } else if fallback_model.is_empty() && !candidate.is_empty() {
                    fallback_model = candidate.to_string();
                }
            }
        }
        if effort.is_empty() {
            if let Some(candidate) = value
                .pointer("/params/output_config/effort")
                .and_then(Value::as_str)
            {
                if matches!(candidate, "low" | "medium" | "high" | "max") {
                    effort = candidate.to_string();
                }
            }
        }
        if !model.is_empty() && !effort.is_empty() {
            break;
        }
    }
    if model.is_empty() {
        model = fallback_model;
    }
    (model, effort)
}

fn claude_argv(launcher: &str, settings: &str, effort: &str) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    if !settings.is_empty() {
        argv.push("--settings".to_string());
        argv.push(settings.to_string());
    }
    if !effort.is_empty() {
        argv.push("--effort".to_string());
        argv.push(effort.to_string());
    }
    if launcher == "claude-ds" && settings.is_empty() {
        // The legacy pending may have the launcher but not the settings path.
        argv.insert(2, "--settings".to_string());
        argv.insert(
            3,
            home_dir()
                .join(".claude/deepseek.json")
                .to_string_lossy()
                .to_string(),
        );
    } else if launcher == "claude-glm" && settings.is_empty() {
        argv.insert(2, "--settings".to_string());
        argv.insert(
            3,
            home_dir()
                .join(".claude/glm.json")
                .to_string_lossy()
                .to_string(),
        );
    }
    argv
}

fn convert_legacy_claude_pending(path: &Path) -> anyhow::Result<PathBuf> {
    let value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let windows = value
        .get("windows")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("legacy pending has no windows"))?;
    let mut new_windows = Vec::new();
    for window in windows {
        let mut tabs = Vec::new();
        let mut active_tab = 0;
        for (index, tab) in window
            .get("tabs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let cwd = tab
                .get("session_cwd")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| tab.get("cwd").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let title = tab
                .get("tab_title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if tab.get("kind").and_then(Value::as_str) == Some("claude") {
                let session_id = tab
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut launcher = tab
                    .get("launcher")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut settings = tab
                    .get("settings_path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut effort = tab
                    .get("effort")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if launcher.is_empty() || effort.is_empty() {
                    let transcript = tab
                        .get("transcript_path")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let (model, inferred_effort) = infer_from_transcript(transcript);
                    if launcher.is_empty() {
                        launcher = if model.to_lowercase().contains("deepseek") {
                            "claude-ds".to_string()
                        } else if model.to_lowercase().contains("glm") {
                            "claude-glm".to_string()
                        } else {
                            "claude".to_string()
                        };
                    }
                    if effort.is_empty() {
                        effort = inferred_effort;
                    }
                    if settings.is_empty() && !session_id.is_empty() {
                        if launcher == "claude-ds" {
                            settings = home_dir()
                                .join(".claude/deepseek.json")
                                .to_string_lossy()
                                .to_string();
                        } else if launcher == "claude-glm" {
                            settings = home_dir()
                                .join(".claude/glm.json")
                                .to_string_lossy()
                                .to_string();
                        }
                    }
                }
                let resume = json!(["--resume", session_id]).to_string();
                let mut user_vars = serde_json::Map::new();
                user_vars.insert("agent.name".to_string(), json!("claude"));
                user_vars.insert("agent.session_id".to_string(), json!(session_id));
                user_vars.insert("agent.resume_args".to_string(), json!(resume));
                user_vars.insert("agent.shutdown".to_string(), json!("1"));
                tabs.push(json!({
                    "title": title,
                    "active_tab": index,
                    "pane": {
                        "cwd": cwd,
                        "argv": claude_argv(&launcher, &settings, &effort),
                        "env": {},
                        "title": tab.get("pane_title").and_then(Value::as_str).unwrap_or(""),
                        "user_vars": user_vars,
                    }
                }));
                if tab
                    .get("is_active_tab")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    active_tab = index;
                }
            } else {
                tabs.push(json!({
                    "title": title,
                    "active_tab": index,
                    "pane": {
                        "cwd": cwd,
                        "argv": [],
                        "env": {},
                        "title": tab.get("pane_title").and_then(Value::as_str).unwrap_or(""),
                        "user_vars": {},
                    }
                }));
                if tab
                    .get("is_active_tab")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    active_tab = index;
                }
            }
        }
        new_windows.push(json!({
            "workspace": window.get("workspace").and_then(Value::as_str).unwrap_or("default"),
            "active_tab": active_tab,
            "tabs": tabs,
        }));
    }
    let converted = json!({
        "version": 1,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "windows": new_windows,
    });
    let temp = std::env::temp_dir().join(format!(
        "wezterm-workspace-legacy-{}.json",
        std::process::id()
    ));
    std::fs::write(&temp, serde_json::to_vec_pretty(&converted)?)?;
    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_claude_conversion_keeps_provider_and_resume() {
        let dir = std::env::temp_dir().join(format!("wezterm-legacy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("session.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"api-request\",\"params\":{\"model\":\"deepseek-v4.1-flash-1017\",",
                "\"output_config\":{\"effort\":\"max\"}}}\n"
            ),
        )
        .unwrap();
        let legacy_path = dir.join("pending.json");
        let legacy = json!({
            "version": 1,
            "windows": [{
                "tabs": [{
                    "kind": "claude",
                    "cwd": "C:/proj",
                    "session_cwd": "C:/proj",
                    "session_id": "11111111-2222-3333-4444-555555555555",
                    "transcript_path": transcript.to_string_lossy(),
                    "tab_title": "tab",
                    "pane_title": "agent",
                    "is_active_tab": true,
                }]
            }]
        });
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let converted_path = convert_legacy_claude_pending(&legacy_path).unwrap();
        let converted: Value =
            serde_json::from_slice(&std::fs::read(&converted_path).unwrap()).unwrap();
        let pane = &converted["windows"][0]["tabs"][0]["pane"];
        let argv = pane["argv"].as_array().unwrap();
        let argv: Vec<_> = argv.iter().filter_map(Value::as_str).collect();
        assert!(argv.contains(&"--settings"));
        assert!(argv.iter().any(|value| value.ends_with("deepseek.json")));
        assert!(argv.contains(&"--effort"));
        assert!(argv.contains(&"max"));
        assert_eq!(
            pane["user_vars"]["agent.resume_args"],
            json!("[\"--resume\",\"11111111-2222-3333-4444-555555555555\"]")
        );
        let _ = std::fs::remove_file(converted_path);
        let _ = std::fs::remove_dir_all(dir);
    }
}
