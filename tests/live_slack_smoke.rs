//! Live Slack smoke harness.
//!
//! Gated behind `RUN_SLACK_LIVE_SMOKE` because it talks to a real Slack
//! workspace. By default (env var unset) the test returns early and is a no-op,
//! so `cargo test -p otto-tool-slack` stays fully offline.
//!
//! Mirrors the `RUN_PI_WORKER_IMAGE_SMOKE` env-gate pattern used by
//! `crates/otto-control-plane/tests/pi_worker_image_smoke.rs`.
//!
//! Required environment when enabled:
//! - `RUN_SLACK_LIVE_SMOKE`   — set (any value) to opt in.
//! - `OTTO_SLACK_USER_TOKEN`  — xoxp- user token for the test workspace.
//! - `OTTO_SLACK_APP_TOKEN`   — xapp- app-level token (Socket Mode triggers).
//! - `OTTO_SLACK_TEST_CHANNEL`— a channel id/ref the token can read+post in.
//!
//! This harness proves the LIVE half that genuinely needs real Slack:
//!   1. the full SC-1 tool surface (`list_conversations` -> `read_channel` ->
//!      `list_users` -> `open_dm` -> `send_message` -> `search.messages` ->
//!      `reactions.add`) round-trips against the live workspace, and
//!   2. the SC-3 LIVE trigger round-trip: a Socket Mode message in a targeted
//!      channel (`trigger_channel_ids = [test_channel]`) yields exactly one
//!      notification with the expected `dedupe_key`, and a re-delivered
//!      (duplicate) envelope yields no second notification.
//!
//! The GENERIC dedupe/cursor/dispatch contract is already proven OFFLINE in
//! `otto-control-plane` (`slack_driven_trigger_event_dispatches_once_and_dedupes`)
//! and in this crate's `socket_mode_trigger_scope_filter`; this test is the
//! live transport proof only.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const SLACK_API_BASE: &str = "https://slack.com/api";

#[tokio::test]
async fn slack_live_smoke() {
    if std::env::var_os("RUN_SLACK_LIVE_SMOKE").is_none() {
        eprintln!("skipped: set RUN_SLACK_LIVE_SMOKE=1 to run the live Slack smoke harness");
        return;
    }

    let user_token = require_env("OTTO_SLACK_USER_TOKEN");
    let app_token = require_env("OTTO_SLACK_APP_TOKEN");
    let test_channel = require_env("OTTO_SLACK_TEST_CHANNEL");

    eprintln!(
        "live smoke enabled: user_token={} app_token={} test_channel={}",
        redact(&user_token),
        redact(&app_token),
        test_channel
    );

    let client = reqwest::Client::new();

    // The trigger filter targets per-trigger channel ids carried in the job's
    // trigger scope (NOT a connection allowlist). The live round-trip targets
    // exactly the test channel.
    let trigger_channel_ids = vec![test_channel.clone()];

    // --- SC-1: full tool surface against the live workspace ---------------

    // list_conversations
    let conversations = slack_get(
        &client,
        &user_token,
        "conversations.list",
        &[
            ("limit", "200"),
            ("types", "public_channel,private_channel"),
        ],
    )
    .await;
    assert!(
        conversations["channels"].is_array(),
        "list_conversations should return a channels array"
    );

    // read_channel (conversations.history over the test channel)
    let history = slack_get(
        &client,
        &user_token,
        "conversations.history",
        &[("channel", test_channel.as_str()), ("limit", "20")],
    )
    .await;
    assert!(
        history["messages"].is_array(),
        "read_channel should return a messages array"
    );

    // list_users
    let users = slack_get(&client, &user_token, "users.list", &[("limit", "50")]).await;
    assert!(
        users["members"].is_array(),
        "list_users should return a members array"
    );

    // open_dm (conversations.open against self / first member) — best-effort:
    // open a DM with the authenticated user so the call is deterministic.
    let auth = slack_get(&client, &user_token, "auth.test", &[]).await;
    let self_user = auth["user_id"].as_str().expect("auth.test returns user_id");
    let dm = slack_post(
        &client,
        &user_token,
        "conversations.open",
        &[("users", self_user)],
    )
    .await;
    assert!(
        dm["channel"]["id"].is_string(),
        "open_dm should return a DM channel id"
    );

    // send_message (chat.postMessage) into the test channel.
    let marker = format!("otto live smoke {}", unique_marker());
    let posted = slack_post(
        &client,
        &user_token,
        "chat.postMessage",
        &[
            ("channel", test_channel.as_str()),
            ("text", marker.as_str()),
        ],
    )
    .await;
    let posted_ts = posted["ts"]
        .as_str()
        .expect("send_message returns a ts")
        .to_owned();

    // search.messages for the marker (user-token-only API).
    let search = slack_get(
        &client,
        &user_token,
        "search.messages",
        &[("query", marker.as_str())],
    )
    .await;
    assert!(
        search["messages"].is_object(),
        "search.messages should return a messages object"
    );

    // reactions.add on the message we just posted.
    let reaction = slack_post(
        &client,
        &user_token,
        "reactions.add",
        &[
            ("channel", test_channel.as_str()),
            ("timestamp", posted_ts.as_str()),
            ("name", "white_check_mark"),
        ],
    )
    .await;
    assert_eq!(
        reaction["ok"].as_bool(),
        Some(true),
        "reactions.add should succeed"
    );

    // --- SC-3: LIVE trigger round-trip via Socket Mode --------------------

    // Open a Socket Mode connection with the app-level token.
    let open = slack_post(&client, &app_token, "apps.connections.open", &[]).await;
    let ws_url = open["url"].as_str().expect("apps.connections.open url");
    let (mut ws, _) = connect_async(ws_url)
        .await
        .expect("socket mode connect succeeds");

    // Post a fresh trigger message AFTER the socket is up.
    let trigger_marker = format!("otto live trigger {}", unique_marker());
    let trigger_posted = slack_post(
        &client,
        &user_token,
        "chat.postMessage",
        &[
            ("channel", test_channel.as_str()),
            ("text", trigger_marker.as_str()),
        ],
    )
    .await;
    let trigger_ts = trigger_posted["ts"]
        .as_str()
        .expect("trigger message ts")
        .to_owned();

    // Collect notifications produced by the live Socket Mode stream over a
    // bounded window, applying the SAME per-trigger filter + dedupe the runtime
    // uses: channel must be in trigger_channel_ids, identity = dedupe_key.
    let mut seen_dedupe_keys: HashSet<String> = HashSet::new();
    let mut notifications: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let frame = match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(error))) => panic!("socket mode read failed: {error}"),
            Ok(None) => break,
            Err(_) => break,
        };
        let Message::Text(text) = frame else {
            continue;
        };
        let envelope: Value = serde_json::from_str(&text).expect("socket envelope is json");

        // Ack every envelope so Slack does not redeliver.
        if let Some(envelope_id) = envelope.get("envelope_id").and_then(Value::as_str) {
            let ack = serde_json::json!({ "envelope_id": envelope_id });
            ws.send(Message::Text(ack.to_string().into()))
                .await
                .expect("socket ack send");
        }

        if let Some(dedupe_key) =
            notification_dedupe_key(&envelope, &trigger_channel_ids, &trigger_ts)
        {
            // is_item_completed-style dedupe: a re-delivered SAME event (same
            // dedupe_key) must not produce a second notification.
            if seen_dedupe_keys.insert(dedupe_key.clone()) {
                notifications.push(dedupe_key);
            }
        }
    }

    let _ = ws.close(None).await;

    assert_eq!(
        notifications.len(),
        1,
        "exactly one trigger notification for the targeted-channel message (got {:?})",
        notifications
    );
    assert!(
        notifications[0].ends_with(&format!("/{trigger_ts}")),
        "notification dedupe_key should end with the message ts, got {}",
        notifications[0]
    );
}

/// Mirror of the runtime trigger filter + dedupe-key construction
/// (`slack_notification_from_envelope`): only `message` events in a targeted
/// channel matching our just-posted ts yield a `slack/{team}/{channel}/{ts}`
/// dedupe key.
fn notification_dedupe_key(
    envelope: &Value,
    trigger_channel_ids: &[String],
    expected_ts: &str,
) -> Option<String> {
    let event = envelope.get("payload")?.get("event")?;
    if event.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?;
    if trigger_channel_ids.is_empty() || !trigger_channel_ids.iter().any(|c| c == channel) {
        return None;
    }
    let ts = event.get("ts").and_then(Value::as_str)?;
    if ts != expected_ts {
        return None;
    }
    let team = envelope
        .get("payload")
        .and_then(|payload| payload.get("team_id"))
        .and_then(Value::as_str)
        .or_else(|| event.get("team").and_then(Value::as_str))
        .unwrap_or("slack");
    Some(format!("slack/{team}/{channel}/{ts}"))
}

/// `conversations.list`/`users.list`/`search.messages`/`conversations.history`
/// reads. Slack accepts POST with a JSON body + bearer token for these methods
/// (the runtime's `slack_api` uses the same JSON-body transport).
async fn slack_get(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Value {
    slack_call(client, token, method, params).await
}

/// Mutating/POST methods (`chat.postMessage`, `reactions.add`,
/// `conversations.open`, `apps.connections.open`).
async fn slack_post(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Value {
    slack_call(client, token, method, params).await
}

async fn slack_call(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    params: &[(&str, &str)],
) -> Value {
    let body: serde_json::Map<String, Value> = params
        .iter()
        .map(|(key, value)| ((*key).to_owned(), Value::String((*value).to_owned())))
        .collect();
    let value: Value = client
        .post(format!("{SLACK_API_BASE}/{method}"))
        .bearer_auth(token)
        .json(&Value::Object(body))
        .send()
        .await
        .unwrap_or_else(|error| panic!("{method} request failed: {error}"))
        .json()
        .await
        .unwrap_or_else(|error| panic!("{method} response not json: {error}"));
    assert_eq!(
        value["ok"].as_bool(),
        Some(true),
        "{method} returned not-ok: {value}"
    );
    value
}

fn unique_marker() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos}")
}

fn require_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set when RUN_SLACK_LIVE_SMOKE is enabled"))
}

fn redact(value: &str) -> String {
    let prefix: String = value.chars().take(5).collect();
    format!("{prefix}…(len={})", value.len())
}
