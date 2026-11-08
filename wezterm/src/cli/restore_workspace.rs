use clap::Parser;
use codec::RestoreWorkspace;
use mux::window::WindowId;
use std::path::PathBuf;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct RestoreWorkspaceCommand {
    /// Read the workspace snapshot from this path. Relative paths are resolved
    /// against the CLI's current directory, not the mux server's.
    #[arg(long)]
    path: PathBuf,

    /// Restore the first saved window into this existing window.
    /// Required together with --replace when more than one window exists.
    #[arg(long)]
    window_id: Option<WindowId>,

    /// Remove the target window's existing tabs after the restored tabs are
    /// spawned. Use this after launching a fresh GUI with one startup pane.
    /// Nothing is removed when no tab could be restored.
    #[arg(long)]
    replace: bool,
}

impl RestoreWorkspaceCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let path = std::env::current_dir()
            .map(|cwd| cwd.join(&self.path))
            .unwrap_or_else(|_| self.path.clone());
        let response = client
            .restore_workspace(RestoreWorkspace {
                path: path.to_string_lossy().to_string(),
                window_id: self.window_id,
                replace: self.replace,
            })
            .await?;
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