use clap::Parser;
use codec::SetUserVar;
use mux::pane::PaneId;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SetUserVarCommand {
    /// Pane to update. Defaults to WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// User var name, for example agent.session_id.
    #[arg(long)]
    name: String,

    /// Value. Omit or pass an empty string to clear the variable.
    #[arg(long)]
    value: Option<String>,

    /// Clear the variable instead of setting it.
    #[arg(long, conflicts_with = "value")]
    unset: bool,
}

impl SetUserVarCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;
        let value = if self.unset {
            None
        } else {
            self.value.clone().filter(|value| !value.is_empty())
        };
        client
            .set_user_var(SetUserVar {
                pane_id,
                name: self.name.clone(),
                value,
            })
            .await?;
        Ok(())
    }
}
