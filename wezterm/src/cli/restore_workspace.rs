use clap::Parser;
use codec::{
    GetCodecVersion, RestoreWorkspace, RestoreWorkspaceDetailed, WORKSPACE_DETAILS_CODEC_VERSION,
};
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

    /// Replace the target window's tabs after all of its panes are restored.
    /// Use this after launching a fresh GUI with one startup pane.
    #[arg(long)]
    replace: bool,
}

impl RestoreWorkspaceCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let path = super::resolve_relative_path(&self.path)?;
        let version = client.get_codec_version(GetCodecVersion {}).await?;
        if version.codec_vers >= WORKSPACE_DETAILS_CODEC_VERSION {
            let response = client
                .restore_workspace_detailed(RestoreWorkspaceDetailed {
                    path,
                    window_id: self.window_id,
                    replace: self.replace,
                })
                .await?;
            println!(
                "WEZTERM-WORKSPACE: restored windows={} tabs={} failed={} panes={} failed_panes={} replaced={}",
                response.windows, response.tabs, response.failed, response.panes,
                response.failed_panes, response.replaced
            );
            for detail in &response.diagnostics {
                eprintln!("restore-workspace: {detail}");
            }
            if response.failed > 0 || response.failed_panes > 0 {
                anyhow::bail!(
                    "workspace restore incomplete: {} tab failure(s), {} missing pane(s)",
                    response.failed,
                    response.failed_panes
                );
            }
        } else {
            if self.replace {
                anyhow::bail!("restore-workspace --replace requires an updated mux server");
            }
            let response = client
                .restore_workspace(RestoreWorkspace {
                    path,
                    window_id: self.window_id,
                    replace: false,
                })
                .await?;
            println!(
                "WEZTERM-WORKSPACE: restored windows={} tabs={} failed={}",
                response.windows, response.tabs, response.failed
            );
            if response.failed > 0 {
                anyhow::bail!("{} tab(s) failed to restore", response.failed);
            }
        }
        Ok(())
    }
}
