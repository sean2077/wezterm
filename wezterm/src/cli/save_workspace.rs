use clap::Parser;
use codec::SaveWorkspace;
use std::path::PathBuf;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SaveWorkspaceCommand {
    /// Write the workspace snapshot to this path.
    /// Defaults to the WezTerm data directory.
    #[arg(long)]
    path: Option<PathBuf>,
}

impl SaveWorkspaceCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let response = client
            .save_workspace(SaveWorkspace {
                path: self
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
            })
            .await?;
        println!(
            "WEZTERM-WORKSPACE: saved windows={} tabs={} -> {}",
            response.windows, response.tabs, response.path
        );
        Ok(())
    }
}
