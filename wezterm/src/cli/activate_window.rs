use clap::Parser;
use codec::SetFocusedPane;
use mux::window::WindowId;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct ActivateWindow {
    /// Specify the target window by its id.
    /// The id is the WINID column from `wezterm cli list`.
    #[arg(long)]
    window_id: WindowId,
}

impl ActivateWindow {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let panes = client.list_panes().await?;
        let active_tab_id = panes.active_tab_ids.get(&self.window_id).copied();

        // Prefer the active pane of the window's active tab, then any pane of
        // that tab, then any pane in the window. Sending SetFocusedPane also
        // brings the owning GUI window to the foreground.
        let mut active_tab_pane = None;
        let mut any_pane = None;
        for tabroot in panes.tabs {
            let mut cursor = tabroot.into_tree().cursor();
            loop {
                if let Some(entry) = cursor.leaf_mut() {
                    if entry.window_id == self.window_id {
                        if Some(entry.tab_id) == active_tab_id {
                            if entry.is_active_pane {
                                active_tab_pane = Some(entry.pane_id);
                            } else if active_tab_pane.is_none() {
                                active_tab_pane = Some(entry.pane_id);
                            }
                        }
                        if any_pane.is_none() {
                            any_pane = Some(entry.pane_id);
                        }
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }

        let pane_id = active_tab_pane
            .or(any_pane)
            .ok_or_else(|| anyhow::anyhow!("window {} has no panes", self.window_id))?;
        client
            .set_focused_pane_id(SetFocusedPane { pane_id })
            .await?;
        Ok(())
    }
}
