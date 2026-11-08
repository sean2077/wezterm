use clap::Parser;
use codec::SetUserVar;
use mux::pane::PaneId;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SetUserVarCommand {
    /// Pane to update. Defaults to WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// Legacy form: user var name, for example agent.session_id.
    #[arg(long, required_unless_present_any = ["vars", "unset"])]
    name: Option<String>,

    /// Legacy form: value. Omit or pass an empty string to clear the variable.
    /// With --unset this must be omitted.
    #[arg(long, conflicts_with = "unset")]
    value: Option<String>,

    /// Set NAME=VALUE. May be repeated.
    #[arg(long = "var", value_name = "NAME=VALUE", action = clap::ArgAction::Append)]
    vars: Vec<String>,

    /// Clear the named variable. May be repeated. A bare --unset clears the
    /// legacy --name variable.
    #[arg(long, num_args = 0..=1, default_missing_value = "", action = clap::ArgAction::Append)]
    unset: Vec<String>,
}

impl SetUserVarCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let pane_id = client.resolve_pane_id(self.pane_id).await?;
        let mut updates: Vec<(&str, Option<&str>)> = Vec::new();

        for spec in &self.vars {
            let (name, value) = spec
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--var expects NAME=VALUE, got {spec:?}"))?;
            if name.is_empty() {
                anyhow::bail!("--var expects a non-empty name");
            }
            updates.push((name, (!value.is_empty()).then_some(value)));
        }

        for name in &self.unset {
            if name.is_empty() {
                // Bare --unset: apply to the legacy --name variable.
                let name = self
                    .name
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("bare --unset requires --name"))?;
                if name.is_empty() {
                    anyhow::bail!("--name must not be empty");
                }
                updates.push((name, None));
            } else {
                updates.push((name, None));
            }
        }

        if let Some(name) = self.name.as_deref() {
            if self.vars.is_empty() && self.unset.is_empty() {
                if name.is_empty() {
                    anyhow::bail!("--name must not be empty");
                }
                let value = self.value.as_deref().filter(|value| !value.is_empty());
                updates.push((name, value));
            }
        }

        if updates.is_empty() {
            anyhow::bail!("nothing to do: pass --var NAME=VALUE or --unset NAME");
        }

        for (name, value) in updates {
            client
                .set_user_var(SetUserVar {
                    pane_id,
                    name: name.to_string(),
                    value: value.map(|value| value.to_string()),
                })
                .await?;
        }
        Ok(())
    }
}