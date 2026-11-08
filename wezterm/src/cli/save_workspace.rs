use clap::Parser;
use codec::SaveWorkspace;
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
        let path = self.path.as_ref().map(|path| {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.clone())
        });
        let response = client
            .save_workspace(SaveWorkspace {
                path: path.map(|path| path.to_string_lossy().to_string()),
            })
            .await?;
        println!(
            "WEZTERM-WORKSPACE: saved windows={} tabs={} -> {}",
            response.windows, response.tabs, response.path
        );
        Ok(())
    }
}
