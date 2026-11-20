//! Native macOS notifications for incoming pairing requests.
//!
//! When running as a `.app` bundle, notifications use `UNUserNotificationCenter`
//! with Approve and Deny actions. When running unbundled (for example via
//! `effigy app:dev`), `UNUserNotificationCenter` refuses to initialize without a
//! bundle identifier, so notifications fall back to `osascript` `display
//! notification` while the window carries Approve and Deny.

use crate::machines::PendingPairPrompt;

/// Delivers native notifications for newly discovered pending pairing requests.
pub trait PairNotifier: Send + Sync + 'static {
    fn notify(&self, prompt: &PendingPairPrompt);
}

/// No-op notifier for tests and non-macOS builds.
#[derive(Default)]
pub struct NoopNotifier;

impl PairNotifier for NoopNotifier {
    fn notify(&self, _prompt: &PendingPairPrompt) {}
}

#[cfg(target_os = "macos")]
pub use macos::MacOsPairNotifier;

#[cfg(not(target_os = "macos"))]
pub type MacOsPairNotifier = NoopNotifier;

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AnyThread, DefinedClass};
    use objc2_foundation::{
        NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSSet, NSString,
    };
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
        UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
        UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
        UNNotificationSound, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    use crate::machines::PendingPairPrompt;

    use super::PairNotifier;

    const CATEGORY_ID: &str = "barge.pair-request";
    const ACTION_APPROVE: &str = "barge.pair-approve";
    const ACTION_DENY: &str = "barge.pair-deny";

    pub type ActionHandler = Arc<dyn Fn(&str, &str, bool) + Send + Sync>;

    struct DelegateIvars {
        requests: Arc<Mutex<HashMap<String, String>>>,
        on_action: ActionHandler,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = AnyThread]
        #[ivars = DelegateIvars]
        struct PairNotificationDelegate;

        unsafe impl NSObjectProtocol for PairNotificationDelegate {}

        unsafe impl UNUserNotificationCenterDelegate for PairNotificationDelegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                completion_handler.call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List
                    | UNNotificationPresentationOptions::Sound,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive_response(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                completion_handler: &block2::DynBlock<dyn Fn()>,
            ) {
                let action_id = response.actionIdentifier().to_string();
                let approve = match action_id.as_str() {
                    ACTION_APPROVE => Some(true),
                    ACTION_DENY => Some(false),
                    _ => None,
                };
                if let Some(approve) = approve {
                    let request_id = response.notification().request().identifier().to_string();
                    let machine_name = self
                        .ivars()
                        .requests
                        .lock()
                        .ok()
                        .and_then(|mut map| map.remove(&request_id));
                    if let Some(machine_name) = machine_name {
                        let handler = Arc::clone(&self.ivars().on_action);
                        std::thread::spawn(move || {
                            handler(&machine_name, &request_id, approve);
                        });
                    }
                }
                completion_handler.call(());
            }
        }
    );

    impl PairNotificationDelegate {
        fn new(
            requests: Arc<Mutex<HashMap<String, String>>>,
            on_action: ActionHandler,
        ) -> Retained<Self> {
            let this = Self::alloc().set_ivars(DelegateIvars {
                requests,
                on_action,
            });
            // SAFETY: `NSObject::init` initializes an allocated `PairNotificationDelegate`.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// macOS notification center sender (with `osascript` fallback when unbundled).
    pub struct MacOsPairNotifier {
        requests: Arc<Mutex<HashMap<String, String>>>,
        on_action: ActionHandler,
        delegate: OnceLock<Retained<PairNotificationDelegate>>,
    }

    impl MacOsPairNotifier {
        pub fn new(on_action: ActionHandler) -> Self {
            Self {
                requests: Arc::new(Mutex::new(HashMap::new())),
                on_action,
                delegate: OnceLock::new(),
            }
        }

        fn ensure_center(&self) -> Option<Retained<UNUserNotificationCenter>> {
            NSBundle::mainBundle().bundleIdentifier()?;
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = self.delegate.get_or_init(|| {
                let delegate =
                    PairNotificationDelegate::new(Arc::clone(&self.requests), Arc::clone(&self.on_action));
                center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                let approve = UNNotificationAction::actionWithIdentifier_title_options(
                    &NSString::from_str(ACTION_APPROVE),
                    &NSString::from_str("Approve"),
                    UNNotificationActionOptions::empty(),
                );
                let deny = UNNotificationAction::actionWithIdentifier_title_options(
                    &NSString::from_str(ACTION_DENY),
                    &NSString::from_str("Deny"),
                    UNNotificationActionOptions::Destructive,
                );
                let actions = NSArray::from_retained_slice(&[approve, deny]);
                let intents: Retained<NSArray<NSString>> = NSArray::new();
                let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                    &NSString::from_str(CATEGORY_ID),
                    &actions,
                    &intents,
                    UNNotificationCategoryOptions::empty(),
                );
                let categories = NSSet::from_retained_slice(&[category]);
                center.setNotificationCategories(&categories);
                let auth_block = block2::RcBlock::new(|_granted: objc2::runtime::Bool, _err: *mut NSError| {});
                center.requestAuthorizationWithOptions_completionHandler(
                    UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                    &auth_block,
                );
                delegate
            });
            let _ = delegate;
            Some(center)
        }
    }

    impl PairNotifier for MacOsPairNotifier {
        fn notify(&self, prompt: &PendingPairPrompt) {
            if let Ok(mut map) = self.requests.lock() {
                map.insert(prompt.request_id.clone(), prompt.machine_name.clone());
            }
            if let Some(center) = self.ensure_center() {
                let content = UNMutableNotificationContent::new();
                content.setTitle(&NSString::from_str(&format!(
                    "{} wants to pair",
                    prompt.client_name
                )));
                content.setSubtitle(&NSString::from_str(&prompt.machine_name));
                content.setBody(&NSString::from_str(&format!(
                    "Verification code: {}",
                    prompt.code
                )));
                content.setCategoryIdentifier(&NSString::from_str(CATEGORY_ID));
                content.setSound(Some(&UNNotificationSound::defaultSound()));
                let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                    &NSString::from_str(&prompt.request_id),
                    &content,
                    None,
                );
                let block = block2::RcBlock::new(|_err: *mut NSError| {});
                center.addNotificationRequest_withCompletionHandler(&request, Some(&block));
                return;
            }
            send_osascript_notification(prompt);
        }
    }

    fn send_osascript_notification(prompt: &PendingPairPrompt) {
        let client = prompt.client_name.clone();
        let machine = prompt.machine_name.clone();
        let code = prompt.code.clone();
        std::thread::spawn(move || {
            let script = "on run argv\nset clientName to item 1 of argv\nset machineName to item 2 of argv\nset codeText to item 3 of argv\ndisplay notification (\"Verification code: \" & codeText & \" — Open Barge to approve or deny\") with title (clientName & \" wants to pair\") subtitle machineName\nend run";
            let _ = std::process::Command::new("osascript")
                .args(["-e", script, "--", &client, &machine, &code])
                .status();
        });
    }
}
