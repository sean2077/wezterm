use clap::Parser;
use codec::{
    GetCodecVersion, SaveWorkspace, SaveWorkspaceDetailed, WORKSPACE_DETAILS_CODEC_VERSION,
};
use std::path::PathBuf;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SaveWorkspaceCommand {
    /// Write the workspace snapshot to this path. Relative paths are resolved
    /// against the CLI's current directory, not the mux server's.
    /// Defaults to the WezTerm data directory.
    #[arg(long)]
    path: Option<PathBuf>,
}

impl SaveWorkspaceCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let path = self
            .path
            .as_deref()
            .map(super::resolve_relative_path)
            .transpose()?;
        let version = client.get_codec_version(GetCodecVersion {}).await?;
        if version.codec_vers >= WORKSPACE_DETAILS_CODEC_VERSION {
            let response = client
                .save_workspace_detailed(SaveWorkspaceDetailed { path })
                .await?;
            println!(
                "WEZTERM-WORKSPACE: saved windows={} tabs={} panes={} skipped={} -> {}",
                response.windows, response.tabs, response.panes, response.skipped, response.path
            );
        } else {
            let response = client.save_workspace(SaveWorkspace { path }).await?;
            println!(
                "WEZTERM-WORKSPACE: saved windows={} tabs={} -> {}",
                response.windows, response.tabs, response.path
            );
        }
        Ok(())
    }
}
