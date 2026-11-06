use clap::Parser;
use codec::CloseAgentSessions;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct CloseAgentSessionsCommand {
    /// How long to wait for agent metadata to be cleared / panes to exit.
    #[arg(long, default_value = "10")]
    timeout_seconds: u64,

    /// Return success even when some sessions remain.
    #[arg(long)]
    force: bool,
}

impl CloseAgentSessionsCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let response = client
            .close_agent_sessions(CloseAgentSessions {
                timeout_seconds: self.timeout_seconds,
                force: self.force,
            })
            .await?;
        println!(
            "WEZTERM-WORKSPACE: close-agent-sessions closed={} remaining={}",
            response.closed, response.remaining
        );
        if response.remaining > 0 && !self.force {
            anyhow::bail!(
                "{} agent session(s) did not exit gracefully; rerun with --force to continue",
                response.remaining
            );
        }
        Ok(())
    }
}
