//! See <https://specifications.freedesktop.org/notification/latest/>

#![cfg(all(not(target_os = "macos"), not(windows)))]

use crate::ToastNotification;
use async_channel::{Receiver, Sender};
use futures_util::{pin_mut, select_biased, FutureExt, TryStreamExt};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use zbus::{fdo::DBusProxy, message::Type, proxy, MatchRule, MessageStream};
use zvariant::Value;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const MAX_REQUESTS: usize = 128;
const MAX_PENDING_ACTIONS: usize = 256;

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
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

struct Request {
    notification: ToastNotification,
    on_click: Option<ClickHandler>,
}

struct PendingAction {
    action: Action,
    token: Option<String>,
    posted_at: Instant,
}

#[derive(Default)]
struct PendingActions(HashMap<String, HashMap<u32, PendingAction>>);

impl PendingActions {
    fn insert(&mut self, owner: String, id: u32, action: Action) {
        if self.0.values().map(HashMap::len).sum::<usize>() >= MAX_PENDING_ACTIONS {
            if let Some((owner, id, _)) = self
                .0
                .iter()
                .flat_map(|(owner, entries)| {
                    entries
                        .iter()
                        .map(move |(id, entry)| (owner.clone(), *id, entry.posted_at))
                })
                .min_by_key(|(_, _, posted_at)| *posted_at)
            {
                self.remove(&owner, id);
            }
        }
        self.0.entry(owner).or_default().insert(
            id,
            PendingAction {
                action,
                token: None,
                posted_at: Instant::now(),
            },
        );
    }

    fn remove(&mut self, owner: &str, id: u32) -> Option<PendingAction> {
        let entries = self.0.get_mut(owner)?;
        let entry = entries.remove(&id);
        if entries.is_empty() {
            self.0.remove(owner);
        }
        entry
    }

    fn set_token(&mut self, owner: &str, id: u32, token: String) {
        if !token.is_empty() {
            if let Some(entry) = self
                .0
                .get_mut(owner)
                .and_then(|entries| entries.get_mut(&id))
            {
                entry.token = Some(token);
            }
        }
    }

    fn take_action(&mut self, owner: &str, id: u32, key: &str) -> Option<PendingAction> {
        let entry = self.0.get(owner)?.get(&id)?;
        if entry.action.key_and_label()[0] == key {
            self.remove(owner, id)
        } else {
            None
        }
    }

    fn owner_changed(&mut self, old_owner: &str) {
        self.0.remove(old_owner);
    }
}

async fn post(
    dbus: &DBusProxy<'_>,
    service: &NotificationsProxy<'_>,
    pending: &mut PendingActions,
    request: Request,
) -> zbus::Result<()> {
    let notif = request.notification;
    let mut action = request
        .on_click
        .map(Action::Activate)
        .or(notif.url.map(Action::OpenUrl));
    let mut action_proxy = None;
    if action.is_some() {
        // If needed, call through the well-known name first to activate the service.
        let name = service.inner().destination();
        let owner = match dbus.get_name_owner(name.clone()).await {
            Ok(owner) => owner,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
                service.get_capabilities().await?;
                dbus.get_name_owner(name.clone()).await?
            }
            Err(err) => return Err(err.into()),
        };
        // Pin Notify and its capability check to the same owner. An ID from a
        // daemon that took over the name must never invoke the old action.
        let proxy = NotificationsProxy::builder(service.inner().connection())
            .destination(owner.clone())?
            .build()
            .await?;
        if proxy
            .get_capabilities()
            .await?
            .iter()
            .any(|cap| cap == "actions")
        {
            action_proxy = Some((owner, proxy));
        } else if matches!(action, Some(Action::OpenUrl(_))) {
            // URL toasts may explicitly ask the user to click their button.
            return Ok(());
        } else {
            action = None;
        }
    }

    let proxy = action_proxy.as_ref().map_or(service, |(_, proxy)| proxy);
    let hints = HashMap::from([("urgency", Value::U8(1 /* Normal */))]);
    let id = proxy
        .notify(
            "wezterm",
            0,
            "org.wezfurlong.wezterm",
            &notif.title,
            &notif.message,
            action.as_ref().map_or(&[], Action::key_and_label),
            &hints,
            notif
                .timeout
                .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
                .unwrap_or(0),
        )
        .await?;
    if let (Some((owner, _)), Some(action)) = (action_proxy, action) {
        pending.insert(owner.as_str().to_owned(), id, action);
    }
    Ok(())
}

fn dispatch_signal(pending: &mut PendingActions, signal: zbus::Message) -> zbus::Result<()> {
    let header = signal.header();
    let Some(sender) = header.sender().map(|name| name.as_str()) else {
        return Ok(());
    };
    let member = header.member().map(|name| name.as_str());
    match (header.interface().map(|name| name.as_str()), member) {
        (Some("org.freedesktop.DBus"), Some("NameOwnerChanged"))
            if sender == "org.freedesktop.DBus" =>
        {
            let (name, old_owner, _): (String, String, String) = signal.body().deserialize()?;
            if name == SERVICE {
                pending.owner_changed(&old_owner);
            }
        }
        (Some(SERVICE), member) if header.path().is_some_and(|path| path.as_str() == PATH) => {
            match member {
                Some("ActivationToken") => {
                    let (id, token): (u32, String) = signal.body().deserialize()?;
                    pending.set_token(sender, id, token);
                }
                Some("ActionInvoked") => {
                    let (id, key): (u32, String) = signal.body().deserialize()?;
                    if let Some(entry) = pending.take_action(sender, id, &key) {
                        match entry.action {
                            Action::OpenUrl(url) => wezterm_open_url::open_url(&url),
                            Action::Activate(on_click) => on_click(entry.token),
                        }
                    }
                }
                Some("NotificationClosed") => {
                    let (id, _): (u32, u32) = signal.body().deserialize()?;
                    pending.remove(sender, id);
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

async fn run_connection(requests: &Receiver<Request>, first: Request) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let dbus = DBusProxy::new(&connection).await?;
    let service = NotificationsProxy::new(&connection).await?;
    let mut signals = MessageStream::from(&connection);
    signals.set_max_queued(MAX_REQUESTS);
    // Use a single stream so owner changes, activations, tokens and closes
    // are observed in bus order, including signals queued during Notify.
    dbus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(SERVICE)?
            .interface(SERVICE)?
            .path(PATH)?
            .build(),
    )
    .await?;
    dbus.add_match_rule(
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, SERVICE)?
            .build(),
    )
    .await?;

    let mut pending = PendingActions::default();
    let mut first = Some(first);
    loop {
        let next = async {
            match first.take() {
                Some(request) => Ok(request),
                None => requests.recv().await,
            }
        }
        .fuse();
        let signal = signals.try_next().fuse();
        pin_mut!(next, signal);
        select_biased! {
            message = signal => {
                let Some(message) = message? else { return Ok(()); };
                if message.message_type() == Type::Signal {
                    if let Err(err) = dispatch_signal(&mut pending, message) {
                        log::debug!("ignoring invalid notification signal: {err:#}");
                    }
                }
            }
            request = next => {
                let Ok(request) = request else { return Ok(()); };
                let posting = post(&dbus, &service, &mut pending, request).fuse();
                let timeout = async_io::Timer::after(Duration::from_secs(5)).fuse();
                pin_mut!(posting, timeout);
                select_biased! {
                    result = posting => {
                        if let Err(err) = result {
                            log::error!("while showing notification: {err:#}");
                        }
                    }
                    _ = timeout => log::warn!("timed out posting desktop notification"),
                }
            }
        }
    }
}

fn show_in_background(notification: ToastNotification, on_click: Option<ClickHandler>) {
    static SENDER: OnceLock<Sender<Request>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded(MAX_REQUESTS);
        let worker = std::thread::Builder::new()
            .name("desktop-notifications".into())
            .spawn(move || {
                async_io::block_on(async move {
                    while let Ok(first) = receiver.recv().await {
                        if let Err(err) = run_connection(&receiver, first).await {
                            log::error!("while connecting to desktop notifications: {err:#}");
                        }
                    }
                });
            });
        if let Err(err) = worker {
            log::error!("starting desktop notification worker: {err:#}");
        }
        sender
    });
    if sender
        .try_send(Request {
            notification,
            on_click,
        })
        .is_err()
    {
        log::debug!("desktop notification queue unavailable");
    }
}

pub fn show_notif(notification: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
    show_in_background(notification, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::object_server::SignalContext;

    #[test]
    fn activation_requires_matching_owner_id_and_key() {
        let mut pending = PendingActions::default();
        pending.insert(":1.1".into(), 4, Action::Activate(Box::new(|_| {})));
        pending.insert(":1.2".into(), 4, Action::Activate(Box::new(|_| {})));
        pending.set_token(":1.1", 4, "old token".into());
        pending.set_token(":1.2", 4, "new token".into());
        pending.owner_changed(":1.1");
        assert!(pending.take_action(":1.1", 4, "default").is_none());
        assert!(pending.take_action(":1.2", 4, "show").is_none());
        assert_eq!(
            pending
                .take_action(":1.2", 4, "default")
                .unwrap()
                .token
                .as_deref(),
            Some("new token")
        );
        assert!(pending.take_action(":1.2", 4, "default").is_none());
    }

    #[test]
    fn close_releases_a_pending_action_and_click_can_lack_a_token() {
        let mut pending = PendingActions::default();
        pending.insert(":1.1".into(), 4, Action::Activate(Box::new(|_| {})));
        pending.insert(":1.1".into(), 5, Action::Activate(Box::new(|_| {})));
        pending.remove(":1.1", 4);
        assert!(pending.take_action(":1.1", 4, "default").is_none());
        assert!(pending
            .take_action(":1.1", 5, "default")
            .unwrap()
            .token
            .is_none());
    }

    struct NotificationService;

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl NotificationService {
        fn get_capabilities(&self) -> Vec<&str> {
            vec!["actions"]
        }

        #[allow(clippy::too_many_arguments)]
        async fn notify(
            &self,
            _app_name: &str,
            _replaces_id: u32,
            _app_icon: &str,
            _summary: &str,
            _body: &str,
            actions: Vec<String>,
            _hints: HashMap<String, zvariant::OwnedValue>,
            _expire_timeout: i32,
            #[zbus(signal_context)] context: SignalContext<'_>,
        ) -> zbus::fdo::Result<u32> {
            assert_eq!(actions, ["default", "Open pane"]);
            Self::activation_token(&context, 42, "test-token").await?;
            Self::action_invoked(&context, 42, "default").await?;
            Ok(42)
        }

        #[zbus(signal)]
        async fn activation_token(
            context: &SignalContext<'_>,
            id: u32,
            token: &str,
        ) -> zbus::Result<()>;

        #[zbus(signal)]
        async fn action_invoked(
            context: &SignalContext<'_>,
            id: u32,
            key: &str,
        ) -> zbus::Result<()>;
    }

    #[test]
    #[ignore = "run under dbus-run-session with WEZTERM_TEST_NOTIFICATIONS_DBUS=1"]
    fn immediate_click_keeps_the_activation_token() {
        assert_eq!(
            std::env::var("WEZTERM_TEST_NOTIFICATIONS_DBUS").as_deref(),
            Ok("1")
        );
        async_io::block_on(async {
            let _service = zbus::connection::Builder::session()
                .unwrap()
                .name(SERVICE)
                .unwrap()
                .serve_at(PATH, NotificationService)
                .unwrap()
                .build()
                .await
                .unwrap();
            let (send, receive) = async_channel::bounded(1);
            persistent_toast_notification_with_click_action("Example", "Message", move |token| {
                send.try_send(token).unwrap();
            });
            let click = receive.recv().fuse();
            let timeout = async_io::Timer::after(Duration::from_secs(5)).fuse();
            pin_mut!(click, timeout);
            let token = select_biased! {
                result = click => result.unwrap(),
                _ = timeout => panic!("the fake notification service's immediate click was lost"),
            };
            assert_eq!(token.as_deref(), Some("test-token"));
        });
    }
}
