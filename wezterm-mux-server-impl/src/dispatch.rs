use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::Async;
use wezterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

/// True when the error means the peer went away. Windows reports a normal
/// client close as ConnectionReset (WSAECONNRESET 10054), not EOF/BrokenPipe,
/// so all disconnect variants must be treated as a clean session end.
fn is_disconnect_io(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
    )
}

fn is_disconnect(err: &anyhow::Error) -> bool {
    err.root_cause()
        .downcast_ref::<std::io::Error>()
        .map(is_disconnect_io)
        .unwrap_or(false)
}

/// Encode and flush one PDU. Returns Ok(false) when the client disconnected.
async fn send_pdu<W>(stream: &mut W, pdu: Pdu, serial: u64) -> anyhow::Result<bool>
where
    W: smol::io::AsyncWriteExt + Unpin,
{
    if let Err(err) = pdu.encode_async(stream, serial).await {
        if is_disconnect(&err) {
            return Ok(false);
        }
        return Err(err).context("encoding PDU to client");
    }
    match stream.flush().await {
        Ok(()) => Ok(true),
        Err(err) if is_disconnect_io(&err) => Ok(false),
        Err(err) => Err(err).context("flushing PDU to client"),
    }
}

pub async fn process_async<T>(mut stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (item_tx, item_rx) = smol::channel::unbounded::<Item>();

    let pdu_sender = PduSender::new({
        let item_tx = item_tx.clone();
        move |pdu| {
            item_tx
                .try_send(Item::WritePdu(pdu))
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let mut handler = SessionHandler::new(pdu_sender);

    {
        let mux = Mux::get();
        let tx = item_tx.clone();
        mux.subscribe(move |n| tx.try_send(Item::Notif(n)).is_ok());
    }

    loop {
        let rx_msg = item_rx.recv();
        let wait_for_read = stream.readable().map(|_| Ok(Item::Readable));

        match smol::future::or(rx_msg, wait_for_read).await {
            Ok(Item::Readable) => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if is_disconnect(&err) {
                            // Client disconnected: no need to make a noise
                            return Ok(());
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                handler.process_one(decoded);
            }
            Ok(Item::WritePdu(decoded)) => {
                if !send_pdu(&mut stream, decoded.pdu, decoded.serial).await? {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::PaneOutput(pane_id))) => {
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::PaneAdded(_pane_id))) => {}
            Ok(Item::Notif(MuxNotification::PaneRemoved(pane_id))) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::PaneRemoved(codec::PaneRemoved { pane_id }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::Alert { pane_id, alert })) => {
                {
                    let per_pane = handler.per_pane(pane_id);
                    let mut per_pane = per_pane.lock().unwrap();
                    per_pane.notifications.push(alert);
                }
                handler.schedule_pane_push(pane_id);
            }
            Ok(Item::Notif(MuxNotification::SaveToDownloads { .. })) => {}
            Ok(Item::Notif(MuxNotification::AssignClipboard {
                pane_id,
                selection,
                clipboard,
            })) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::SetClipboard(codec::SetClipboard {
                        pane_id,
                        clipboard,
                        selection,
                    }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id })) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WindowRemoved(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowCreated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowInvalidated(_window_id))) => {}
            Ok(Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id))) => {
                let workspace = {
                    let mux = Mux::get();
                    mux.get_window(window_id)
                        .map(|w| w.get_workspace().to_string())
                };
                if let Some(workspace) = workspace {
                    if !send_pdu(
                        &mut stream,
                        Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                            window_id,
                            workspace,
                        }),
                        0,
                    )
                    .await?
                    {
                        return Ok(());
                    }
                }
            }
            Ok(Item::Notif(MuxNotification::PaneFocused(pane_id))) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::PaneFocused(codec::PaneFocused { pane_id }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            // Process-local: a remote GUI client has no PDU for this and is
            // not expected to raise its OS window from a mux notification.
            Ok(Item::Notif(MuxNotification::WindowActivationRequested(_))) => {}
            Ok(Item::Notif(MuxNotification::TabResized(tab_id))) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::TabResized(codec::TabResized { tab_id }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::TabTitleChanged { tab_id, title })) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WindowTitleChanged { window_id, title })) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            })) => {
                if !send_pdu(
                    &mut stream,
                    Pdu::RenameWorkspace(codec::RenameWorkspace {
                        old_workspace,
                        new_workspace,
                    }),
                    0,
                )
                .await?
                {
                    return Ok(());
                }
            }
            Ok(Item::Notif(MuxNotification::ActiveWorkspaceChanged(_))) => {}
            Ok(Item::Notif(MuxNotification::Empty)) => {}
            Err(err) => {
                // The notification channel closed (the mux subscription is
                // gone); client-side disconnects are handled where reads and
                // writes fail.
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_error_kinds_are_clean() {
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            assert!(is_disconnect_io(&std::io::Error::from(kind)), "{:?}", kind);
            let wrapped = anyhow::Error::new(std::io::Error::from(kind));
            assert!(is_disconnect(&wrapped), "{:?}", kind);
        }
        assert!(!is_disconnect_io(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!is_disconnect(&anyhow::anyhow!("not io")));
    }
}
