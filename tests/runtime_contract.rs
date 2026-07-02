use otto_extension_sdk::protocol::{
    METHOD_HANDSHAKE, METHOD_HEALTH, METHOD_REGISTRATIONS_GET, METHOD_SETUP_CHECKS_RUN,
    METHOD_SHUTDOWN, METHOD_TOOLS_INVOKE, METHOD_TRIGGER_FORM_CONFIGURATION,
};
use otto_extension_sdk::rpc::framing::{read_rpc_frame, write_rpc_frame};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const PACKAGE_ID: &str = "com.otto.slack";
const SETUP_READY: &str = "setup.default.slack.ready";
const RUN_ID: &str = "00000000-0000-0000-0000-000000000065";
const GRANT_ID: &str = "00000000-0000-0000-0000-000000000071";
const TOOL_READ_THREAD: &str = "tool.default.slack.read_thread";
const TOOL_LIST_USERS: &str = "tool.default.slack.list_users";
const TOOL_SEARCH_MESSAGES: &str = "tool.default.slack.search_messages";
const TOOL_ADD_REACTION: &str = "tool.default.slack.add_reaction";
const TOOL_SEND_MESSAGE: &str = "tool.default.slack.send_message";

#[tokio::test]
async fn slack_tool_runtime_fake_mode() -> anyhow::Result<()> {
    let mut process = spawn_slack()?;
    assert_runtime_ready(&mut process).await?;
    shutdown(process).await
}

#[tokio::test]
async fn slack_tool_runtime_invokes_fake_tools() -> anyhow::Result<()> {
    let mut process = spawn_slack()?;
    assert_fake_tool_invocations(&mut process).await?;
    shutdown(process).await
}

async fn assert_runtime_ready(process: &mut SlackProcess) -> anyhow::Result<()> {
    let handshake = request(
        &mut process.stdin,
        &mut process.stdout,
        1,
        METHOD_HANDSHAKE,
        Some(json!({
            "protocol_version": "otto.extension.rpc.v1",
            "package_id": PACKAGE_ID
        })),
    )
    .await?;
    assert_eq!(handshake["result"]["package_id"], PACKAGE_ID);
    assert_eq!(handshake["result"]["display_name"], "Default Slack Tools");

    let health = request(
        &mut process.stdin,
        &mut process.stdout,
        2,
        METHOD_HEALTH,
        None,
    )
    .await?;
    assert_eq!(health["result"]["healthy"], true);
    assert_eq!(health["result"]["status"], "ok");

    let setup = request(
        &mut process.stdin,
        &mut process.stdout,
        3,
        METHOD_SETUP_CHECKS_RUN,
        Some(json!({ "setup_check_id": SETUP_READY })),
    )
    .await?;
    assert_eq!(setup["result"]["setup_check_id"], SETUP_READY);
    assert_eq!(setup["result"]["ok"], true);
    assert_eq!(setup["result"]["details"]["fake_mode"], true);
    assert_eq!(
        setup["result"]["details"]["workspace_ref"],
        "workspace.fake"
    );
    assert_eq!(
        setup["result"]["details"]["channel_refs"],
        json!(["channel.prod-targeting-alerts"])
    );
    let setup_payload = setup.to_string();
    assert!(!setup_payload.contains("token"));

    let registrations = request(
        &mut process.stdin,
        &mut process.stdout,
        4,
        METHOD_REGISTRATIONS_GET,
        None,
    )
    .await?;
    assert_manifest_registration_ceiling(&registrations["result"]["registrations"]);

    let trigger_form = request(
        &mut process.stdin,
        &mut process.stdout,
        5,
        METHOD_TRIGGER_FORM_CONFIGURATION,
        Some(json!({
            "connection": {
                "package_id": PACKAGE_ID,
                "connection_id": null,
                "name": "e2e_bot",
                "alias_prefix": "e2e_bot",
                "account_hint": null
            },
            "config": {},
            "trigger_id": "trigger.default.slack.message"
        })),
    )
    .await?;
    assert_eq!(
        trigger_form["result"]["form"]["fields"][0]["name"],
        "trigger_channel_ids"
    );
    assert_eq!(
        trigger_form["result"]["form"]["fields"][0]["options_call"],
        "list_channels"
    );
    assert_eq!(trigger_form["result"]["calls"][0]["id"], "list_channels");

    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn assert_fake_tool_invocations(process: &mut SlackProcess) -> anyhow::Result<()> {
    let read_thread = request(
        &mut process.stdin,
        &mut process.stdout,
        20,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_READ_THREAD,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "read",
            "package_scope": {
                "mode": "read",
                "workspace_ref": "workspace.fake",
                "channel_refs": ["channel.prod-targeting-alerts"],
                "max_messages": 3
            },
            "arguments": {
                "workspace_ref": "workspace.fake",
                "channel_ref": "channel.prod-targeting-alerts",
                "thread_ref": "thread.1710000000.000100",
                "max_messages": 3
            }
        })),
    )
    .await?;
    assert_eq!(read_thread["result"]["is_error"], false);
    assert_eq!(
        read_thread["result"]["content"][0]["text"],
        "Read 2 synthetic Slack thread messages from fake runtime."
    );
    assert_eq!(
        read_thread["result"]["structured_content"]["tool"],
        "read_thread"
    );
    assert_eq!(
        read_thread["result"]["structured_content"]["thread"]["thread_ref"],
        "thread.1710000000.000100"
    );
    assert_eq!(
        read_thread["result"]["structured_content"]["messages"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    let list_users = request(
        &mut process.stdin,
        &mut process.stdout,
        21,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_LIST_USERS,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "read",
            "package_scope": {
                "mode": "read",
                "workspace_ref": "workspace.fake",
                "channel_refs": ["channel.prod-targeting-alerts"],
                "max_messages": 3
            },
            "arguments": {
                "workspace_ref": "workspace.fake",
                "query": "alert"
            }
        })),
    )
    .await?;
    assert_eq!(list_users["result"]["is_error"], false);
    assert_eq!(
        list_users["result"]["content"][0]["text"],
        "Read 2 synthetic Slack thread messages from fake runtime."
    );

    let search_messages = request(
        &mut process.stdin,
        &mut process.stdout,
        24,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_SEARCH_MESSAGES,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "read",
            "package_scope": {
                "mode": "read",
                "workspace_ref": "workspace.fake"
            },
            "arguments": {
                "query": "targeting alert"
            }
        })),
    )
    .await?;
    assert_eq!(search_messages["result"]["is_error"], false);
    assert_eq!(
        search_messages["result"]["content"][0]["text"],
        "Found 2 synthetic Slack search matches from fake runtime."
    );
    assert_eq!(
        search_messages["result"]["structured_content"]["tool"],
        "search_messages"
    );
    assert_eq!(
        search_messages["result"]["structured_content"]["query"],
        "targeting alert"
    );
    assert_eq!(
        search_messages["result"]["structured_content"]["match_count"],
        2
    );

    // search_messages is a read-only tool; a mismatched send scope
    // scope must be rejected with slack_tool_mode_mismatch.
    let search_mode_mismatch = request(
        &mut process.stdin,
        &mut process.stdout,
        26,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_SEARCH_MESSAGES,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "send",
            "package_scope": {
                "mode": "send",
                "workspace_ref": "workspace.fake"
            },
            "arguments": {
                "query": "targeting alert"
            }
        })),
    )
    .await?;
    assert_eq!(
        search_mode_mismatch["error"]["message"],
        "slack_tool_mode_mismatch"
    );

    let add_reaction = request(
        &mut process.stdin,
        &mut process.stdout,
        25,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_ADD_REACTION,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "send",
            "package_scope": {
                "mode": "send",
                "workspace_ref": "workspace.fake"
            },
            "arguments": {
                "channel": "C0FAKECHANNEL",
                "timestamp": "1710000000.000100",
                "name": "white_check_mark"
            }
        })),
    )
    .await?;
    assert_eq!(add_reaction["result"]["is_error"], false);
    assert_eq!(
        add_reaction["result"]["content"][0]["text"],
        "Added synthetic Slack reaction from fake runtime."
    );
    assert_eq!(
        add_reaction["result"]["structured_content"]["tool"],
        "add_reaction"
    );
    assert_eq!(add_reaction["result"]["structured_content"]["ok"], true);
    assert_eq!(
        add_reaction["result"]["structured_content"]["name"],
        "white_check_mark"
    );

    let send_message = request(
        &mut process.stdin,
        &mut process.stdout,
        22,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_SEND_MESSAGE,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "send",
            "package_scope": {
                "mode": "send",
                "workspace_ref": "workspace.fake",
                "channel_refs": ["channel.prod-targeting-alerts"]
            },
            "arguments": {
                "workspace_ref": "workspace.fake",
                "channel_ref": "channel.prod-targeting-alerts",
                "thread_ref": "thread.1710000000.000100",
                "body": "Synthetic validation-only send request."
            }
        })),
    )
    .await?;
    assert_eq!(send_message["result"]["is_error"], false);
    assert_eq!(
        send_message["result"]["content"][0]["text"],
        "Slack send_message is blocked by the Phase 6 fake runtime; no send attempted."
    );
    assert_eq!(
        send_message["result"]["structured_content"]["tool"],
        "send_message"
    );
    assert_eq!(
        send_message["result"]["structured_content"]["blocked"],
        true
    );
    assert_eq!(send_message["result"]["structured_content"]["sent"], false);

    let mode_mismatch = request(
        &mut process.stdin,
        &mut process.stdout,
        23,
        METHOD_TOOLS_INVOKE,
        Some(json!({
            "tool_id": TOOL_SEND_MESSAGE,
            "run_id": RUN_ID,
            "grant_id": GRANT_ID,
            "mode": "read",
            "package_scope": {
                "mode": "read",
                "workspace_ref": "workspace.fake",
                "channel_refs": ["channel.prod-targeting-alerts"]
            },
            "arguments": {}
        })),
    )
    .await?;
    assert_eq!(
        mode_mismatch["error"]["message"],
        "slack_tool_mode_mismatch"
    );

    Ok(())
}

fn assert_manifest_registration_ceiling(registrations: &Value) {
    assert_eq!(
        registrations["roles"][0]["capabilities"],
        json!([
            "cap.default.slack.read",
            "cap.default.slack.trigger",
            "cap.default.slack.send"
        ])
    );
    let tool_ids = registrations["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["id"].as_str().expect("tool id"))
        .collect::<Vec<_>>();
    assert_eq!(
        tool_ids,
        vec![
            "tool.default.slack.read_thread",
            "tool.default.slack.read_channel",
            "tool.default.slack.read_recent_dms",
            "tool.default.slack.list_conversations",
            "tool.default.slack.list_users",
            "tool.default.slack.open_dm",
            "tool.default.slack.search_messages",
            "tool.default.slack.add_reaction",
            "tool.default.slack.send_message",
        ]
    );
    for tool in registrations["tools"].as_array().expect("tools") {
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|description| !description.trim().is_empty())
        );
    }

    assert_eq!(
        registrations["triggers"],
        json!([{
            "id": "trigger.default.slack.message",
            "display_name": "Slack message trigger",
            "event_schema": "schema.default.slack.message_event",
            "required_capabilities": ["cap.default.slack.trigger"],
            "required_scope_fields": ["trigger_channel_ids"]
        }])
    );

    assert_eq!(
        registrations["setup_checks"],
        json!([{
            "id": SETUP_READY,
            "display_name": "Slack package ready",
            "output_schema": "schema.default.slack.setup_details",
            "required_capabilities": [
                "cap.default.slack.read",
                "cap.default.slack.trigger"
            ]
        }])
    );

    assert_eq!(
        registrations["ui_forms"],
        json!([
            {
                "id": "slack_setup",
                "display_name": "Slack setup",
                "schema": "schema.default.slack.setup_form"
            },
            {
                "id": "slack_grant",
                "display_name": "Slack grant",
                "schema": "schema.default.slack.grant_form"
            }
        ])
    );

    assert_eq!(
        registrations["redaction"],
        json!([{
            "id": "redaction.default.slack.message_content",
            "display_name": "Slack message content redaction",
            "input_schema": "schema.default.slack.redaction_input"
        }])
    );

    assert_eq!(registrations["migrations"], json!([]));
    assert_eq!(registrations["schemas"].as_array().map(Vec::len), Some(12));
    assert_eq!(
        registrations["capabilities"].as_array().map(Vec::len),
        Some(3)
    );
}

struct SlackProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

fn spawn_slack() -> anyhow::Result<SlackProcess> {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin("otto-tool-slack"))
        .arg("--fake")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("slack stdin");
    let stdout = child.stdout.take().expect("slack stdout");
    Ok(SlackProcess {
        child,
        stdin,
        stdout,
    })
}

async fn request(
    stdin: &mut ChildStdin,
    stdout: &mut ChildStdout,
    id: u64,
    method: &str,
    params: Option<Value>,
) -> anyhow::Result<Value> {
    let mut message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method
    });
    if let Some(params) = params {
        message["params"] = params;
    }
    write_rpc_frame(stdin, &message).await?;
    Ok(read_rpc_frame(stdout, 64 * 1024).await?)
}

async fn shutdown(mut process: SlackProcess) -> anyhow::Result<()> {
    let shutdown = request(
        &mut process.stdin,
        &mut process.stdout,
        999,
        METHOD_SHUTDOWN,
        Some(json!({ "reason": "test complete" })),
    )
    .await?;
    assert_eq!(shutdown["result"]["accepted"], true);
    process.stdin.shutdown().await?;
    let status = process.child.wait().await?;
    assert!(status.success());
    Ok(())
}
