//! See <https://specifications.freedesktop.org/notification/latest/>

#![cfg(all(not(target_os = "macos"), not(windows)))]

use crate::ToastNotification;
use futures_util::TryStreamExt;
use std::collections::HashMap;
use zbus::{fdo::DBusProxy, message::Type, proxy, MatchRule, MessageStream};
use zvariant::Value;

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    /// GetCapabilities method
    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

type ClickHandler = Box<dyn FnOnce(Option<String>) + Send>;

enum Action {
    OpenUrl(String),
    Activate(ClickHandler),
}

impl Action {
    fn key_and_label(&self) -> &'static [&'static str] {
        match self {
            Self::OpenUrl(_) => &["show", "Show"],
            Self::Activate(_) => &["default", "Open pane"],
        }
    }
}

async fn show_notif_impl(
    notif: ToastNotification,
    on_click: Option<ClickHandler>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;
    let mut proxy = NotificationsProxy::new(&connection).await?;

    let action = on_click
        .map(Action::Activate)
        .or(notif.url.map(Action::OpenUrl));
    let action = match action {
        Some(action)
            if !proxy
                .get_capabilities()
                .await?
                .iter()
                .any(|cap| cap == "actions") =>
        {
            // URL toasts may explicitly ask the user to click their button.
            if matches!(action, Action::OpenUrl(_)) {
                return Ok(());
            }
            None
        }
        action => action,
    };

    // Subscribe before posting so even an immediate click is observed. Use one
    // stream to preserve the order of the token, action, and close signals.
    let mut signals = if action.is_some() {
        let owner = DBusProxy::new(&connection)
            .await?
            .get_name_owner(proxy.inner().destination().clone())
            .await?;
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(owner.clone())?
            .interface(proxy.inner().interface().clone())?
            .path(proxy.inner().path().clone())?
            .build();
        let signals = MessageStream::for_match_rule(rule, &connection, None).await?;
        proxy = NotificationsProxy::builder(&connection)
            .destination(owner)?
            .build()
            .await?;
        Some(signals)
    } else {
        None
    };

    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(1 /* Normal */));
    let notification = proxy
        .notify(
            "wezterm",
            0,
            "org.wezfurlong.wezterm",
            &notif.title,
            &notif.message,
            action.as_ref().map_or(&[], Action::key_and_label),
            &hints,
            notif.timeout.map(|d| d.as_millis() as _).unwrap_or(0),
        )
        .await?;

    if let (Some(signals), Some(action)) = (&mut signals, action) {
        let mut activation_token = None;
        while let Some(signal) = signals.try_next().await? {
            match signal.header().member().map(|name| name.as_str()) {
                Some("ActivationToken") => {
                    let (id, token): (u32, String) = signal.body().deserialize()?;
                    if id == notification && !token.is_empty() {
                        activation_token = Some(token);
                    }
                }
                Some("ActionInvoked") => {
                    let (id, key): (u32, String) = signal.body().deserialize()?;
                    if id == notification && key == action.key_and_label()[0] {
                        match action {
                            Action::OpenUrl(url) => wezterm_open_url::open_url(&url),
                            Action::Activate(on_click) => on_click(activation_token),
                        }
                        break;
                    }
                }
                Some("NotificationClosed") => {
                    let (id, _reason): (u32, u32) = signal.body().deserialize()?;
                    if id == notification {
                        break;
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn show_in_background(notif: ToastNotification, on_click: Option<ClickHandler>) {
    // Run this in a separate thread so the notification service cannot block the GUI.
    std::thread::spawn(move || {
        let res = async_io::block_on(show_notif_impl(notif, on_click));
        if let Err(err) = res {
            log::error!("while showing notification: {:#}", err);
        }
    });
}

pub fn show_notif(notif: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
    show_in_background(notif, None);
    Ok(())
}

pub fn persistent_toast_notification_with_click_action(
    title: &str,
    message: &str,
    on_click: impl FnOnce(Option<String>) + Send + 'static,
) {
    show_in_background(
        ToastNotification {
            title: title.to_string(),
            message: message.to_string(),
            url: None,
            timeout: None,
        },
        Some(Box::new(on_click)),
    );
}
