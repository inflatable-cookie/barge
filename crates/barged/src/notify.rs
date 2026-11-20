//! Desktop notification for incoming pairing requests through
//! `org.freedesktop.Notifications` on the session bus.
//!
//! Hidden behind [`PairingNotifier`] so tests exercise the pairing/approval
//! flow deterministically without D-Bus. When no notification service is
//! running (for example in SteamOS Gaming Mode), notification is a quiet no-op.

use std::sync::Arc;

/// Details sent to the desktop notification service for a new pairing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairNotification {
    pub request_id: String,
    pub client_name: String,
    pub code: String,
}

/// Action clicked by the operator on a pairing notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairAction {
    Approve,
    Deny,
}

/// Callback invoked when the operator acts on a notification.
pub type ActionCallback = Arc<dyn Fn(&str, PairAction) + Send + Sync>;

/// Sends pairing notifications with Approve and Deny actions.
pub trait PairingNotifier: Send + Sync + 'static {
    fn notify(&self, notice: PairNotification, on_action: ActionCallback);
}

/// Session-bus `org.freedesktop.Notifications` notifier (Linux only).
#[cfg(target_os = "linux")]
#[derive(Debug, Default, Clone, Copy)]
pub struct DesktopNotifier;

#[cfg(target_os = "linux")]
impl PairingNotifier for DesktopNotifier {
    fn notify(&self, notice: PairNotification, on_action: ActionCallback) {
        std::thread::Builder::new()
            .name("barged-notify".to_string())
            .spawn(move || {
                send_freedesktop_notification(&notice, on_action);
            })
            .ok();
    }
}

#[cfg(target_os = "linux")]
fn send_freedesktop_notification(notice: &PairNotification, on_action: ActionCallback) {
    use std::collections::HashMap;
    use zbus::zvariant::Value;

    let Ok(connection) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    ) else {
        return;
    };
    let mut signals = proxy.receive_signal("ActionInvoked").ok();
    let summary = format!("{} wants to pair", notice.client_name);
    let body = format!("Verification code: {}", notice.code);
    let actions = vec!["approve", "Approve", "deny", "Deny"];
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let Ok(reply) = connection.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "barged",
            0_u32,
            "",
            summary.as_str(),
            body.as_str(),
            actions,
            hints,
            120_000_i32,
        ),
    ) else {
        // Gaming Mode or headless session without a notification service: quiet no-op.
        return;
    };
    let Ok(notification_id) = reply.body().deserialize::<u32>() else {
        return;
    };
    let Some(ref mut iter) = signals else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(125);
    for signal in iter {
        if std::time::Instant::now() >= deadline {
            break;
        }
        let Ok((id, action_key)): std::result::Result<(u32, String), _> =
            signal.body().deserialize()
        else {
            continue;
        };
        if id != notification_id {
            continue;
        }
        match action_key.as_str() {
            "approve" => {
                on_action(&notice.request_id, PairAction::Approve);
                break;
            }
            "deny" => {
                on_action(&notice.request_id, PairAction::Deny);
                break;
            }
            _ => {}
        }
    }
}

/// Non-Linux no-op notifier.
#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopNotifier;

#[cfg(not(target_os = "linux"))]
impl PairingNotifier for NoopNotifier {
    fn notify(&self, _notice: PairNotification, _on_action: ActionCallback) {}
}
