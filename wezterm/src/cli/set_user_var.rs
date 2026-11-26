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
    #[arg(long, required_unless_present_any = ["vars", "unset"], conflicts_with = "vars")]
    name: Option<String>,

    /// Legacy form: value. Omit or pass an empty string to clear the variable.
    /// With --unset this must be omitted.
    #[arg(long, requires = "name", conflicts_with_all = ["unset", "vars"])]
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
        let updates = self.updates()?;
        let pane_id = client.resolve_pane_id(self.pane_id).await?;

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

    fn updates(&self) -> anyhow::Result<Vec<(&str, Option<&str>)>> {
        if self.name.is_some() && self.unset.iter().any(|name| !name.is_empty()) {
            anyhow::bail!("--name only accepts a bare --unset; use --unset NAME without --name for batch updates");
        }
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

        Ok(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_legacy_and_batch_arguments_are_rejected() {
        for args in [
            vec!["--name", "one", "--var", "two=value"],
            vec!["--value", "ignored", "--var", "two=value"],
            vec!["--name", "one", "--value", "ignored", "--var", "two=value"],
        ] {
            assert!(
                SetUserVarCommand::try_parse_from(std::iter::once("set-user-var").chain(args))
                    .is_err()
            );
        }
    }

    #[test]
    fn legacy_set_and_clear_remain_supported() {
        let set = SetUserVarCommand::try_parse_from([
            "set-user-var",
            "--name",
            "agent.session_id",
            "--value",
            "session",
        ])
        .unwrap();
        assert_eq!(
            set.updates().unwrap(),
            [("agent.session_id", Some("session"))]
        );
        for args in [
            vec!["--name", "agent.session_id"],
            vec!["--name", "agent.session_id", "--unset"],
            vec!["--name", "agent.session_id", "--value", ""],
        ] {
            let clear =
                SetUserVarCommand::try_parse_from(std::iter::once("set-user-var").chain(args))
                    .unwrap();
            assert_eq!(clear.updates().unwrap(), [("agent.session_id", None)]);
        }
    }

    #[test]
    fn batch_set_and_clear_preserve_values_with_equals() {
        let cmd = SetUserVarCommand::try_parse_from([
            "set-user-var",
            "--var",
            "one=a=b",
            "--var",
            "empty=",
            "--unset",
            "old",
        ])
        .unwrap();
        assert_eq!(
            cmd.updates().unwrap(),
            [("one", Some("a=b")), ("empty", None), ("old", None)]
        );
    }

    #[test]
    fn ambiguous_or_invalid_updates_fail_before_sending() {
        for args in [
            vec!["--name", "ignored", "--unset", "another"],
            vec!["--unset"],
            vec!["--var", "valid=value", "--var", "missing-equals"],
            vec!["--var", "=empty-name"],
            vec!["--name", ""],
        ] {
            let cmd =
                SetUserVarCommand::try_parse_from(std::iter::once("set-user-var").chain(args))
                    .unwrap();
            assert!(cmd.updates().is_err());
        }
    }
}
