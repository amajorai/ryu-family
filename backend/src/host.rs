//! Family sidecar → Core callback for the `notify.deliver` kernel capability.
//!
//! `notify.deliver` needs no `host_api` grant: the minted sidecar token plus an
//! enabled app is enough. On an org-bound node Core checks the target against
//! the org roster and answers 403 for a non-member. There is deliberately no
//! email path: Core pins `email.send` to `@ryu/mail`.

use std::time::Duration;

use serde::Serialize;

use crate::state::PLUGIN_ID;

pub const MAX_TITLE_BYTES: usize = 256;
pub const MAX_BODY_BYTES: usize = 4_000;
const DELIVER_TIMEOUT: Duration = Duration::from_secs(15);

/// One reminder for one recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// `None` for the `"local"` recipient: Core then delivers to the node's
    /// active account.
    pub target_user_id: Option<String>,
}

/// What the sweep does with a claim after a delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverOutcome {
    /// 200: keep the claim and record the notification id.
    Delivered(Option<String>),
    /// 4xx: a non-member, a disabled app or a validation error. Keep the claim
    /// as `failed`; retrying would fail the same way.
    Rejected(u16),
    /// 5xx, timeout or network error: release the claim so a later tick on the
    /// same local day retries.
    Retry(String),
}

#[derive(Serialize)]
struct DeliverBody<'a> {
    title: &'a str,
    body: &'a str,
    level: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_user_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct NotifyHost {
    client: reqwest::Client,
    base: String,
    token: String,
    plugin_id: String,
}

impl NotifyHost {
    /// The Core callback, or `None` when Core did not inject `RYU_CORE_PORT` and
    /// `RYU_EXT_TOKEN` (a standalone run): the sweep then claims nothing.
    pub fn from_env() -> Option<Self> {
        let port = std::env::var("RYU_CORE_PORT")
            .ok()
            .map(|port| port.trim().to_owned())
            .filter(|port| !port.is_empty())?;
        let token = std::env::var("RYU_EXT_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())?;
        let plugin_id = std::env::var("RYU_EXT_PLUGIN_ID")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| PLUGIN_ID.to_owned());
        Some(Self::new(
            format!("http://127.0.0.1:{port}"),
            token,
            plugin_id,
        ))
    }

    /// A callback against an explicit base URL (tests point this at a local stub).
    pub fn new(
        base: impl Into<String>,
        token: impl Into<String>,
        plugin_id: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            plugin_id: plugin_id.into(),
        }
    }

    /// Exactly one `notify.deliver` call.
    pub async fn deliver(&self, notification: &Notification) -> DeliverOutcome {
        let body = DeliverBody {
            title: truncate_bytes(&notification.title, MAX_TITLE_BYTES),
            body: truncate_bytes(&notification.body, MAX_BODY_BYTES),
            level: "info",
            target_user_id: notification.target_user_id.as_deref(),
        };
        let response = self
            .client
            .post(format!("{}/api/host/capability/notify.deliver", self.base))
            .bearer_auth(&self.token)
            .header("x-ryu-plugin-id", &self.plugin_id)
            .json(&body)
            .timeout(DELIVER_TIMEOUT)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => return DeliverOutcome::Retry(error.to_string()),
        };
        let status = response.status();
        if status.is_success() {
            let notification_id =
                response
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|value| {
                        value
                            .get("notification_id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    });
            return DeliverOutcome::Delivered(notification_id);
        }
        if status.is_client_error() {
            return DeliverOutcome::Rejected(status.as_u16());
        }
        DeliverOutcome::Retry(format!("Core answered HTTP {status}"))
    }
}

/// The longest prefix of `value` that fits in `max` bytes on a char boundary.
pub fn truncate_bytes(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_bytes("abc", 5), "abc");
        assert_eq!(truncate_bytes("abcdef", 3), "abc");
        // "é" is two bytes; cutting inside it backs off to the boundary.
        assert_eq!(truncate_bytes("aé", 2), "a");
    }

    #[test]
    fn the_local_recipient_omits_target_user_id() {
        let body = DeliverBody {
            title: "t",
            body: "b",
            level: "info",
            target_user_id: None,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert!(value.get("target_user_id").is_none());
        assert_eq!(value["level"], "info");
    }
}
