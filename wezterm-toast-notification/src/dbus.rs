//! See <https://specifications.freedesktop.org/notification/latest/>

#![cfg(all(not(target_os = "macos"), not(windows)))]

use crate::ToastNotification;
use async_channel::{Receiver, Sender};
use futures_util::{pin_mut, select_biased, FutureExt, TryStreamExt};
use std::collections::{HashMap, VecDeque};
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

struct PostedAction {
    owner: String,
    id: u32,
    action: Action,
}

async fn post(
    dbus: &DBusProxy<'_>,
    service: &NotificationsProxy<'_>,
    request: Request,
) -> zbus::Result<Option<PostedAction>> {
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
            return Ok(None);
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
    Ok(match (action_proxy, action) {
        (Some((owner, _)), Some(action)) => Some(PostedAction {
            owner: owner.as_str().to_owned(),
            id,
            action,
        }),
        _ => None,
    })
}

// Signals can arrive before Notify returns its ID. Keep the relevant early
// events while continuing to drain zbus's bounded stream; otherwise a burst
// of signals blocks its socket reader and even the Notify reply cannot arrive.
// Existing actions are still dispatched immediately. Limit the unknown IDs to
// the same budget as pending actions so unrelated applications cannot grow our
// memory without bound.
#[derive(Default)]
struct EarlyNotification {
    token: Option<String>,
    // Only the two keys that we advertise are retained. Each key records the
    // token present at its first invocation, before any subsequent close.
    invoked: HashMap<String, Option<String>>,
    closed: bool,
}

#[derive(Default)]
struct EarlySignals {
    entries: VecDeque<(String, u32, EarlyNotification)>,
    service_changed: bool,
}

impl EarlySignals {
    fn receive(&mut self, pending: &mut PendingActions, signal: zbus::Message) -> zbus::Result<()> {
        let header = signal.header();
        let Some(sender) = header.sender().map(|name| name.as_str()) else {
            return Ok(());
        };
        let member = header.member().map(|name| name.as_str());
        if header
            .interface()
            .is_some_and(|name| name.as_str() == "org.freedesktop.DBus")
            && member == Some("NameOwnerChanged")
            && sender == "org.freedesktop.DBus"
        {
            let (name, old_owner, _): (String, String, String) = signal.body().deserialize()?;
            if name == SERVICE && !old_owner.is_empty() {
                // A daemon disappeared or was replaced during this request.
                // Discard its in-flight action as well as already posted ones.
                self.service_changed = true;
                self.entries.clear();
            }
            return dispatch_signal(pending, signal);
        }
        if !header
            .interface()
            .is_some_and(|name| name.as_str() == SERVICE)
            || !header.path().is_some_and(|path| path.as_str() == PATH)
            || !matches!(
                member,
                Some("ActivationToken" | "ActionInvoked" | "NotificationClosed")
            )
        {
            return Ok(());
        }
        let (id, value) = match member {
            Some("ActivationToken" | "ActionInvoked") => {
                let (id, value): (u32, String) = signal.body().deserialize()?;
                (id, value)
            }
            _ => {
                let (id, _): (u32, u32) = signal.body().deserialize()?;
                (id, String::new())
            }
        };
        if pending
            .0
            .get(sender)
            .is_some_and(|entries| entries.contains_key(&id))
        {
            return dispatch_signal(pending, signal);
        }
        if self.service_changed {
            return Ok(());
        }
        let index = self
            .entries
            .iter()
            .position(|(owner, entry_id, _)| owner == sender && *entry_id == id);
        let mut entry = match index {
            Some(index) => self.entries.remove(index).unwrap(),
            None => (sender.to_owned(), id, EarlyNotification::default()),
        };
        if !entry.2.closed {
            match member {
                Some("ActivationToken") if !value.is_empty() => entry.2.token = Some(value),
                Some("ActionInvoked") if matches!(value.as_str(), "default" | "show") => {
                    let token = entry.2.token.clone();
                    entry.2.invoked.entry(value).or_insert(token);
                }
                Some("NotificationClosed") => entry.2.closed = true,
                _ => {}
            }
        }
        self.entries.push_back(entry);
        if self.entries.len() > MAX_PENDING_ACTIONS {
            // A drained click cannot be replayed, so drop an entry that has
            // not accepted one first. A burst of signals for other
            // notifications must not discard an action we already saw.
            let evict = self
                .entries
                .iter()
                .position(|(_, _, entry)| entry.invoked.is_empty())
                .unwrap_or(0);
            self.entries.remove(evict);
        }
        Ok(())
    }

    fn finish(self, pending: &mut PendingActions, posted: PostedAction) {
        if self.service_changed {
            return;
        }
        let early = self
            .entries
            .into_iter()
            .find(|(owner, id, _)| owner == &posted.owner && *id == posted.id);
        if let Some((_, _, mut early)) = early {
            if let Some(token) = early.invoked.remove(posted.action.key_and_label()[0]) {
                invoke_action(posted.action, token);
                return;
            }
            if early.closed {
                return;
            }
            let owner = posted.owner;
            pending.insert(owner.clone(), posted.id, posted.action);
            if let Some(token) = early.token {
                pending.set_token(&owner, posted.id, token);
            }
        } else {
            pending.insert(posted.owner, posted.id, posted.action);
        }
    }
}

fn invoke_action(action: Action, token: Option<String>) {
    match action {
        Action::OpenUrl(url) => wezterm_open_url::open_url(&url),
        Action::Activate(on_click) => on_click(token),
    }
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
                        invoke_action(entry.action, entry.token);
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
    // Register matches before activating the bounded message stream. No
    // notification has been posted yet, and an undrained stream could otherwise
    // block the reply to the second AddMatch during a signal burst.
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

    // Use a single stream so owner changes, activations, tokens and closes
    // are observed in bus order, including signals queued during Notify.
    let mut signals = MessageStream::from(&connection);
    signals.set_max_queued(MAX_REQUESTS);
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
                let posting = post(&dbus, &service, request).fuse();
                let timeout = async_io::Timer::after(Duration::from_secs(5)).fuse();
                pin_mut!(posting, timeout);
                let mut early = EarlySignals::default();
                loop {
                    let signal = signals.try_next().fuse();
                    pin_mut!(signal);
                    select_biased! {
                        result = posting => {
                            match result {
                                Ok(Some(posted)) => early.finish(&mut pending, posted),
                                Ok(None) => {},
                                Err(err) => log::error!("while showing notification: {err:#}"),
                            }
                            break;
                        }
                        _ = timeout => {
                            log::warn!("timed out posting desktop notification");
                            break;
                        }
                        message = signal => {
                            let Some(message) = message? else { return Ok(()); };
                            if message.message_type() == Type::Signal {
                                if let Err(err) = early.receive(&mut pending, message) {
                                    log::debug!("ignoring invalid notification signal: {err:#}");
                                }
                            }
                        }
                    }
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

    fn early_signal(member: &str, id: u32, value: &str) -> zbus::Message {
        zbus::Message::signal(PATH, SERVICE, member)
            .unwrap()
            .sender(":1.1")
            .unwrap()
            .build(&(id, value))
            .unwrap()
    }

    fn close_signal(id: u32) -> zbus::Message {
        zbus::Message::signal(PATH, SERVICE, "NotificationClosed")
            .unwrap()
            .sender(":1.1")
            .unwrap()
            .build(&(id, 2u32))
            .unwrap()
    }

    #[test]
    fn early_click_uses_its_token_and_survives_a_following_close() {
        let mut pending = PendingActions::default();
        let mut early = EarlySignals::default();
        for token in ["stale", "stale", "stale", "stale", "click-token"] {
            early
                .receive(&mut pending, early_signal("ActivationToken", 42, token))
                .unwrap();
        }
        early
            .receive(&mut pending, early_signal("ActionInvoked", 42, "default"))
            .unwrap();
        early
            .receive(
                &mut pending,
                early_signal("ActivationToken", 42, "too-late"),
            )
            .unwrap();
        early.receive(&mut pending, close_signal(42)).unwrap();
        let (send, recv) = std::sync::mpsc::channel();
        early.finish(
            &mut pending,
            PostedAction {
                owner: ":1.1".into(),
                id: 42,
                action: Action::Activate(Box::new(move |token| send.send(token).unwrap())),
            },
        );
        assert_eq!(recv.try_recv().unwrap().as_deref(), Some("click-token"));
        assert!(pending.0.is_empty());
    }

    #[test]
    fn early_close_prevents_a_later_click_and_owner_changes_cancel_inflight_actions() {
        let mut pending = PendingActions::default();
        let mut early = EarlySignals::default();
        early.receive(&mut pending, close_signal(42)).unwrap();
        early
            .receive(&mut pending, early_signal("ActionInvoked", 42, "default"))
            .unwrap();
        early.finish(
            &mut pending,
            PostedAction {
                owner: ":1.1".into(),
                id: 42,
                action: Action::Activate(Box::new(|_| panic!("closed notification was activated"))),
            },
        );
        assert!(pending.0.is_empty());

        let mut early = EarlySignals::default();
        early
            .receive(&mut pending, early_signal("ActionInvoked", 42, "default"))
            .unwrap();
        let changed = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .unwrap()
        .sender("org.freedesktop.DBus")
        .unwrap()
        .build(&(SERVICE, ":1.1", ":1.2"))
        .unwrap();
        early.receive(&mut pending, changed).unwrap();
        early.finish(
            &mut pending,
            PostedAction {
                owner: ":1.1".into(),
                id: 42,
                action: Action::Activate(Box::new(|_| {
                    panic!("replaced daemon's action was activated")
                })),
            },
        );
        assert!(pending.0.is_empty());
    }

    #[test]
    fn early_click_survives_eviction_from_a_signal_burst() {
        let mut pending = PendingActions::default();
        let mut early = EarlySignals::default();
        let (send, recv) = std::sync::mpsc::channel();
        early
            .receive(&mut pending, early_signal("ActionInvoked", 42, "default"))
            .unwrap();
        for id in 100..100 + MAX_PENDING_ACTIONS as u32 * 2 {
            early
                .receive(&mut pending, early_signal("ActivationToken", id, "token"))
                .unwrap();
        }
        early.finish(
            &mut pending,
            PostedAction {
                owner: ":1.1".into(),
                id: 42,
                action: Action::Activate(Box::new(move |token| send.send(token).unwrap())),
            },
        );
        assert_eq!(recv.try_recv().unwrap(), None);
        assert!(pending.0.is_empty());
    }

    #[test]
    fn posting_dispatches_existing_actions_and_bounds_unknown_ids() {
        let mut pending = PendingActions::default();
        let mut early = EarlySignals::default();
        let (send, recv) = std::sync::mpsc::channel();
        pending.insert(
            ":1.1".into(),
            7,
            Action::Activate(Box::new(move |token| send.send(token).unwrap())),
        );
        early
            .receive(&mut pending, early_signal("ActionInvoked", 7, "default"))
            .unwrap();
        assert_eq!(recv.try_recv().unwrap(), None);
        for id in 100..100 + MAX_PENDING_ACTIONS as u32 * 2 {
            early
                .receive(&mut pending, early_signal("ActivationToken", id, "token"))
                .unwrap();
        }
        assert_eq!(early.entries.len(), MAX_PENDING_ACTIONS);
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
            summary: &str,
            _body: &str,
            actions: Vec<String>,
            _hints: HashMap<String, zvariant::OwnedValue>,
            _expire_timeout: i32,
            #[zbus(signal_context)] context: SignalContext<'_>,
        ) -> zbus::fdo::Result<u32> {
            assert_eq!(actions, ["default", "Open pane"]);
            if summary == "Signal burst" {
                // Another application's signals can fill the bounded stream
                // before Notify returns. Keep draining it while waiting for
                // the reply, or zbus cannot read the reply from the socket.
                for id in 1000..1000 + MAX_REQUESTS as u32 * 2 {
                    Self::activation_token(&context, id, "unrelated-token").await?;
                }
            }
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
    fn immediate_click_keeps_the_activation_token_during_a_signal_burst() {
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
            for title in ["Example", "Signal burst"] {
                let (send, receive) = async_channel::bounded(1);
                persistent_toast_notification_with_click_action(title, "Message", move |token| {
                    send.try_send(token).unwrap();
                });
                let click = receive.recv().fuse();
                let timeout = async_io::Timer::after(Duration::from_secs(3)).fuse();
                pin_mut!(click, timeout);
                let token = select_biased! {
                    result = click => result.unwrap(),
                    _ = timeout => panic!("the fake notification service's {} click was lost", title),
                };
                assert_eq!(token.as_deref(), Some("test-token"));
            }
        });
    }
}
