//! First-party Slack tool package constants and runtime registrations.

use otto_extension_sdk::extension_ids::{
    CapabilityId, RedactionId, RoleId, SchemaId, SetupCheckId, ToolId, TriggerId, UiFormId,
};
use otto_extension_sdk::grants::CapabilityMode;
use otto_extension_sdk::protocol::{ContentBlock, ToolInvokeParams, ToolInvokeResult};
use otto_extension_sdk::roles::{
    CapabilityDeclaration, ExtensionRegistrations, ExtensionRoleKind, RedactionContributor,
    RoleRegistration, SchemaRegistration, SetupCheckRegistration, ToolRegistration,
    TriggerRegistration, UiFormRegistration,
};
use serde_json::{Value, json};
use std::error::Error;
use std::fmt;

/// Stable extension package ID for the default Slack tool package.
pub const PACKAGE_ID: &str = "com.otto.slack";
/// User-facing package name.
pub const DISPLAY_NAME: &str = "Default Slack Tools";
/// Runtime argument that selects deterministic fake mode.
pub const FAKE_MODE_ARG: &str = "--fake";

/// Tool package role ID.
pub const ROLE_ID: &str = "role.default.tool.slack";
/// Bounded Slack thread read tool ID.
pub const TOOL_READ_THREAD: &str = "tool.default.slack.read_thread";
/// Bounded Slack channel read tool ID.
pub const TOOL_READ_CHANNEL: &str = "tool.default.slack.read_channel";
/// Bounded Slack recent DM read tool ID.
pub const TOOL_READ_DMS: &str = "tool.default.slack.read_recent_dms";
/// Slack conversation list tool ID.
pub const TOOL_LIST_CONVERSATIONS: &str = "tool.default.slack.list_conversations";
/// Slack user list/search tool ID.
pub const TOOL_LIST_USERS: &str = "tool.default.slack.list_users";
/// Slack open DM tool ID.
pub const TOOL_OPEN_DM: &str = "tool.default.slack.open_dm";
/// Slack message search tool ID (user-token + search:read).
pub const TOOL_SEARCH_MESSAGES: &str = "tool.default.slack.search_messages";
/// Slack add-reaction tool ID (reactions:write).
pub const TOOL_ADD_REACTION: &str = "tool.default.slack.add_reaction";
/// Validation-only Slack send tool ID.
pub const TOOL_SEND_MESSAGE: &str = "tool.default.slack.send_message";
/// Slack message trigger ID.
pub const TRIGGER_MESSAGE: &str = "trigger.default.slack.message";
/// Slack readiness setup-check ID.
pub const SETUP_READY: &str = "setup.default.slack.ready";
/// Slack read capability ID.
pub const CAP_READ: &str = "cap.default.slack.read";
/// Slack trigger capability ID.
pub const CAP_TRIGGER: &str = "cap.default.slack.trigger";
/// Slack validation-only send capability ID.
pub const CAP_SEND: &str = "cap.default.slack.send";
/// Slack setup details schema ID.
pub const SCHEMA_SETUP_DETAILS: &str = "schema.default.slack.setup_details";
/// Slack grant scope schema ID.
pub const SCHEMA_GRANT_SCOPE: &str = "schema.default.slack.grant_scope";
/// Slack message event schema ID.
pub const SCHEMA_MESSAGE_EVENT: &str = "schema.default.slack.message_event";
/// Slack read-thread input schema ID.
pub const SCHEMA_READ_THREAD_INPUT: &str = "schema.default.slack.read_thread_input";
/// Slack read-thread output schema ID.
pub const SCHEMA_READ_THREAD_OUTPUT: &str = "schema.default.slack.read_thread_output";
/// Slack search-messages input schema ID.
pub const SCHEMA_SEARCH_MESSAGES_INPUT: &str = "schema.default.slack.search_messages_input";
/// Slack add-reaction input schema ID.
pub const SCHEMA_ADD_REACTION_INPUT: &str = "schema.default.slack.add_reaction_input";
/// Slack send-message input schema ID.
pub const SCHEMA_SEND_MESSAGE_INPUT: &str = "schema.default.slack.send_message_input";
/// Slack send-message output schema ID.
pub const SCHEMA_SEND_MESSAGE_OUTPUT: &str = "schema.default.slack.send_message_output";
/// Slack redaction input schema ID.
pub const SCHEMA_REDACTION_INPUT: &str = "schema.default.slack.redaction_input";
/// Slack setup UI form schema ID.
pub const SCHEMA_SETUP_FORM: &str = "schema.default.slack.setup_form";
/// Slack grant UI form schema ID.
pub const SCHEMA_GRANT_FORM: &str = "schema.default.slack.grant_form";
/// Slack setup UI form ID.
pub const UI_SETUP: &str = "slack_setup";
/// Slack grant UI form ID.
pub const UI_GRANT: &str = "slack_grant";
/// Slack redaction contributor ID.
pub const REDACTION_MESSAGE_CONTENT: &str = "redaction.default.slack.message_content";

/// Error returned by deterministic Slack fake tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRuntimeError {
    message: &'static str,
}

impl ToolRuntimeError {
    const fn new(message: &'static str) -> Self {
        Self { message }
    }

    /// Machine-readable runtime error code.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ToolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl Error for ToolRuntimeError {}

/// Invokes a deterministic fake Slack tool using the same behavior as the runtime binary.
///
/// # Errors
///
/// Returns [`ToolRuntimeError`] when the tool ID is unknown or the invocation mode does not match
/// the compiled package scope.
pub fn invoke_fake_tool(params: &ToolInvokeParams) -> Result<ToolInvokeResult, ToolRuntimeError> {
    match params.tool_id.as_str() {
        TOOL_READ_THREAD => invoke_read_thread(params),
        TOOL_READ_CHANNEL
        | TOOL_READ_DMS
        | TOOL_LIST_CONVERSATIONS
        | TOOL_LIST_USERS
        | TOOL_OPEN_DM => invoke_read_thread(params),
        TOOL_SEARCH_MESSAGES => invoke_search_messages(params),
        TOOL_ADD_REACTION => invoke_add_reaction(params),
        TOOL_SEND_MESSAGE => invoke_send_message(params),
        _ => Err(ToolRuntimeError::new("unknown_slack_tool")),
    }
}

fn ok_result(text: &str, structured: Value) -> ToolInvokeResult {
    ToolInvokeResult {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
            annotations: None,
            _meta: None,
        }],
        structured_content: Some(structured),
        output_schema: None,
        is_error: false,
        idempotency_key: None,
        _meta: None,
    }
}

#[allow(dead_code)]
fn error_result(text: &str) -> ToolInvokeResult {
    ToolInvokeResult {
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
            annotations: None,
            _meta: None,
        }],
        structured_content: None,
        output_schema: None,
        is_error: true,
        idempotency_key: None,
        _meta: None,
    }
}

/// Returns runtime registrations that match `extensions/default-tool-slack/otto.toml`.
#[must_use]
pub fn registrations() -> ExtensionRegistrations {
    let cap_read = capability_id(CAP_READ);
    let cap_trigger = capability_id(CAP_TRIGGER);
    let cap_send = capability_id(CAP_SEND);

    ExtensionRegistrations {
        roles: vec![RoleRegistration {
            id: role_id(ROLE_ID),
            kind: ExtensionRoleKind::ToolPackage,
            display_name: "Default Slack tool package".to_owned(),
            capabilities: vec![cap_read.clone(), cap_trigger.clone(), cap_send.clone()],
        }],
        schemas: schema_registrations(),
        tools: vec![
            ToolRegistration {
                id: tool_id(TOOL_READ_THREAD),
                display_name: "Read Slack thread".to_owned(),
                description: Some("Read messages from a specific Slack thread. Requires a Slack channel ID or channel reference plus a thread timestamp/reference. Use list_conversations for channel discovery and list_users/open_dm for DM discovery before reading a DM thread.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_READ_CHANNEL),
                display_name: "Read Slack channel".to_owned(),
                description: Some("Read recent messages from one Slack channel. Prefer channel_id with a real Slack channel ID such as C.../G.../D..., or channel with a #channel name. Do not pass a human name here; use list_users/open_dm for people and DMs.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_READ_DMS),
                display_name: "Read recent Slack DMs".to_owned(),
                description: Some("Read recent direct-message conversations visible to this Slack token. Use when the user asks about DMs generally. Output includes DM channel/user references that can be used by read/open/send tools.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_LIST_CONVERSATIONS),
                display_name: "List Slack conversations".to_owned(),
                description: Some("List Slack channels, private channels, DMs, and group DMs visible to this token. Use this before reading or sending when the user gives a channel name, DM, or ambiguous conversation reference.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_LIST_USERS),
                display_name: "List Slack users".to_owned(),
                description: Some("Find Slack users visible to this token. Use this before opening a DM or sending a DM when the user gives a person's name. Prefer the returned user_id (U.../W...) in open_dm or send_message.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_OPEN_DM),
                display_name: "Open Slack DM".to_owned(),
                description: Some("Open or resolve a Slack DM channel for a user. Requires user_id when possible; user names can be attempted but list_users is more reliable. Returns a DM channel_id/channel_ref for read_channel or send_message.".to_owned()),
                input_schema: schema_id(SCHEMA_READ_THREAD_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_SEARCH_MESSAGES),
                display_name: "Search Slack messages".to_owned(),
                description: Some("Search workspace messages with a Slack search query (words, in:#channel, from:@user). User-token connections only (requires search:read); resolve names via discovery first using list_conversations/list_users.".to_owned()),
                input_schema: schema_id(SCHEMA_SEARCH_MESSAGES_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_read.clone()],
                requires_approval: Some(false),
                read_only: true,
                destructive: false,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(false)),
            },
            ToolRegistration {
                id: tool_id(TOOL_ADD_REACTION),
                display_name: "Add Slack reaction".to_owned(),
                description: Some("Add an emoji reaction to a message by channel + ts. Provide the emoji name without colons (e.g. white_check_mark).".to_owned()),
                input_schema: schema_id(SCHEMA_ADD_REACTION_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_send.clone()],
                requires_approval: Some(true),
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(true)),
            },
            ToolRegistration {
                id: tool_id(TOOL_SEND_MESSAGE),
                display_name: "Send Slack message".to_owned(),
                description: Some("Send a Slack message only after Otto approval. Requires text/body/message plus a real target: channel_id for channels or user_id for DMs. Set thread_ts to reply within / start a thread. If the user gives a person name like 'John', use list_users first to get the Slack user_id, then call send_message with user_id; do not put a human name in channel_id.".to_owned()),
                input_schema: schema_id(SCHEMA_SEND_MESSAGE_INPUT),
                output_schema: None,
                required_capabilities: vec![cap_send.clone()],
                requires_approval: Some(true),
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: true,
                runtime_commands: slack_runtime_commands(),
                scope_defaults: Some(slack_scope_defaults(true)),
            },
        ],
        triggers: vec![TriggerRegistration {
            id: trigger_id(TRIGGER_MESSAGE),
            display_name: "Slack message trigger".to_owned(),
            event_schema: schema_id(SCHEMA_MESSAGE_EVENT),
            required_capabilities: vec![cap_trigger.clone()],
            required_scope_fields: vec!["trigger_channel_ids".to_owned()],
        }],
        setup_checks: vec![SetupCheckRegistration {
            id: setup_check_id(SETUP_READY),
            display_name: "Slack package ready".to_owned(),
            output_schema: Some(schema_id(SCHEMA_SETUP_DETAILS)),
            required_capabilities: vec![cap_read.clone(), cap_trigger.clone()],
        }],
        ui_forms: vec![
            UiFormRegistration {
                id: ui_form_id(UI_SETUP),
                display_name: "Slack setup".to_owned(),
                schema: schema_id(SCHEMA_SETUP_FORM),
            },
            UiFormRegistration {
                id: ui_form_id(UI_GRANT),
                display_name: "Slack grant".to_owned(),
                schema: schema_id(SCHEMA_GRANT_FORM),
            },
        ],
        migrations: Vec::new(),
        redaction: vec![RedactionContributor {
            id: redaction_id(REDACTION_MESSAGE_CONTENT),
            display_name: "Slack message content redaction".to_owned(),
            input_schema: Some(schema_id(SCHEMA_REDACTION_INPUT)),
        }],
        capabilities: vec![
            CapabilityDeclaration {
                id: cap_read,
                mode: CapabilityMode::Read,
                description: "Read bounded Slack thread content through opaque credential, workspace, and channel references.".to_owned(),
            },
            CapabilityDeclaration {
                id: cap_trigger,
                mode: CapabilityMode::Read,
                description: "Receive bounded Slack message trigger envelopes with cursor and dedupe identity.".to_owned(),
            },
            CapabilityDeclaration {
                id: cap_send,
                mode: CapabilityMode::Send,
                description: "Validate an explicitly approved Slack send request; the reference workflow does not use this capability.".to_owned(),
            },
        ],
    }
}

fn slack_runtime_commands() -> Vec<String> {
    vec!["slack".to_owned(), "slack-cli".to_owned()]
}

fn slack_scope_defaults(allow_send_validation: bool) -> Value {
    json!({
        "thread_scope": "channel_threads",
        "max_thread_messages": 20,
        "max_message_chars": 1200,
        "max_body_chars": 1200,
        "max_source_refs": 8,
        "allow_send_validation": allow_send_validation,
    })
}

type ToolRuntimeResult<T> = Result<T, ToolRuntimeError>;

fn invoke_read_thread(params: &ToolInvokeParams) -> ToolRuntimeResult<ToolInvokeResult> {
    ensure_mode(params, CapabilityMode::Read)?;
    let thread_ref = bounded_arg(
        &params.arguments,
        "thread_ref",
        "thread.1710000000.000100",
        64,
    );
    let channel_ref = bounded_arg(
        &params.arguments,
        "channel_ref",
        "channel.prod-targeting-alerts",
        80,
    );

    Ok(ok_result(
        "Read 2 synthetic Slack thread messages from fake runtime.",
        json!({
            "tool": "read_thread",
            "workspace_ref": "workspace.fake",
            "channel_ref": channel_ref,
            "thread": {
                "thread_ref": thread_ref,
                "root_message_ref": "message.fake.001",
                "message_count": 2
            },
            "messages": [
                {
                    "message_ref": "message.fake.001",
                    "author_ref": "user.fake.alert-bot",
                    "timestamp": "1710000000.000100",
                    "text": "Synthetic targeting alert fixture: error budget burn is elevated."
                },
                {
                    "message_ref": "message.fake.002",
                    "author_ref": "user.fake.oncall",
                    "timestamp": "1710000001.000200",
                    "text": "Synthetic investigation note: checking read-only diagnostics."
                }
            ],
            "truncated": false
        }),
    ))
}

fn invoke_send_message(params: &ToolInvokeParams) -> ToolRuntimeResult<ToolInvokeResult> {
    ensure_mode(params, CapabilityMode::Send)?;

    Ok(ok_result(
        "Slack send_message is blocked by the Phase 6 fake runtime; no send attempted.",
        json!({
            "tool": "send_message",
            "blocked": true,
            "sent": false,
            "reason": "phase_6_validation_only",
            "workspace_ref": "workspace.fake",
            "channel_ref": "channel.prod-targeting-alerts"
        }),
    ))
}

fn invoke_search_messages(params: &ToolInvokeParams) -> ToolRuntimeResult<ToolInvokeResult> {
    ensure_mode(params, CapabilityMode::Search)?;
    let query = bounded_arg(&params.arguments, "query", "targeting alert", 80);

    Ok(ok_result(
        "Found 2 synthetic Slack search matches from fake runtime.",
        json!({
            "tool": "search_messages",
            "workspace_ref": "workspace.fake",
            "query": query,
            "matches": [
                {
                    "message_ref": "message.fake.search.001",
                    "channel_ref": "channel.prod-targeting-alerts",
                    "author_ref": "user.fake.alert-bot",
                    "timestamp": "1710000000.000100",
                    "text": "Synthetic search match: error budget burn is elevated."
                },
                {
                    "message_ref": "message.fake.search.002",
                    "channel_ref": "channel.prod-targeting-alerts",
                    "author_ref": "user.fake.oncall",
                    "timestamp": "1710000001.000200",
                    "text": "Synthetic search match: checking read-only diagnostics."
                }
            ],
            "match_count": 2,
            "truncated": false
        }),
    ))
}

fn invoke_add_reaction(params: &ToolInvokeParams) -> ToolRuntimeResult<ToolInvokeResult> {
    ensure_mode(params, CapabilityMode::Send)?;
    let channel = bounded_arg(&params.arguments, "channel", "C0FAKECHANNEL", 96);
    let timestamp = bounded_arg(&params.arguments, "timestamp", "1710000000.000100", 64);
    let name = bounded_arg(&params.arguments, "name", "white_check_mark", 64);

    Ok(ok_result(
        "Added synthetic Slack reaction from fake runtime.",
        json!({
            "tool": "add_reaction",
            "ok": true,
            "workspace_ref": "workspace.fake",
            "channel_ref": channel_fake_ref(&channel),
            "timestamp": timestamp,
            "name": name
        }),
    ))
}

fn channel_fake_ref(channel: &str) -> String {
    if channel.starts_with("channel.") {
        channel.to_owned()
    } else {
        format!("channel.{channel}")
    }
}

fn ensure_mode(params: &ToolInvokeParams, expected: CapabilityMode) -> ToolRuntimeResult<()> {
    if params.mode != expected {
        return Err(ToolRuntimeError::new("slack_tool_mode_mismatch"));
    }

    let Some(scope_mode) = params.package_scope.get("mode").and_then(Value::as_str) else {
        return Err(ToolRuntimeError::new("slack_package_scope_mode_missing"));
    };
    if scope_mode != mode_name(expected) {
        return Err(ToolRuntimeError::new("slack_tool_mode_mismatch"));
    }

    Ok(())
}

fn mode_name(mode: CapabilityMode) -> &'static str {
    match mode {
        CapabilityMode::Read => "read",
        CapabilityMode::Search => "search",
        CapabilityMode::Write => "write",
        CapabilityMode::Draft => "draft",
        CapabilityMode::Send => "send",
        CapabilityMode::Mutate => "mutate",
        CapabilityMode::Exec => "exec",
    }
}

fn bounded_arg(arguments: &Value, key: &str, fallback: &str, max_chars: usize) -> String {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    value.chars().take(max_chars).collect()
}

fn schema_registrations() -> Vec<SchemaRegistration> {
    vec![
        schema_registration(
            SCHEMA_SETUP_DETAILS,
            "schemas/setup-details.schema.json",
            "Slack setup-check details for real Web API and Socket Mode readiness.",
        ),
        schema_registration(
            SCHEMA_GRANT_SCOPE,
            "schemas/grant-scope.schema.json",
            "Slack tool package grant scope with credential, workspace, channel, and bounded content limits.",
        ),
        schema_registration(
            SCHEMA_MESSAGE_EVENT,
            "schemas/message-event.schema.json",
            "Slack message trigger envelope with cursor and dedupe identity.",
        ),
        schema_registration(
            SCHEMA_READ_THREAD_INPUT,
            "schemas/read-thread-input.schema.json",
            "Input for bounded Slack thread reads.",
        ),
        schema_registration(
            SCHEMA_READ_THREAD_OUTPUT,
            "schemas/read-thread-output.schema.json",
            "Output for bounded Slack thread reads.",
        ),
        schema_registration(
            SCHEMA_SEARCH_MESSAGES_INPUT,
            "schemas/search-messages-input.schema.json",
            "Input for searching Slack workspace messages (query, count).",
        ),
        schema_registration(
            SCHEMA_ADD_REACTION_INPUT,
            "schemas/add-reaction-input.schema.json",
            "Input for adding an emoji reaction to a Slack message (channel, timestamp, name).",
        ),
        schema_registration(
            SCHEMA_SEND_MESSAGE_INPUT,
            "schemas/send-message-input.schema.json",
            "Validation-only input for an explicitly approved Slack send request.",
        ),
        schema_registration(
            SCHEMA_SEND_MESSAGE_OUTPUT,
            "schemas/send-message-output.schema.json",
            "Validation-only output for a Slack send request.",
        ),
        schema_registration(
            SCHEMA_REDACTION_INPUT,
            "schemas/redaction-input.schema.json",
            "Slack redaction input for bounded message content and credential-like references.",
        ),
        schema_registration(
            SCHEMA_SETUP_FORM,
            "ui/setup.form.json",
            "Slack setup form fixture.",
        ),
        schema_registration(
            SCHEMA_GRANT_FORM,
            "ui/grant.form.json",
            "Slack grant form fixture.",
        ),
    ]
}

fn schema_registration(id: &str, path: &str, description: &str) -> SchemaRegistration {
    SchemaRegistration {
        id: schema_id(id),
        path: path.to_owned(),
        description: Some(description.to_owned()),
    }
}

fn capability_id(value: &str) -> CapabilityId {
    CapabilityId::new(value).expect("valid Slack capability id")
}

fn redaction_id(value: &str) -> RedactionId {
    RedactionId::new(value).expect("valid Slack redaction id")
}

fn role_id(value: &str) -> RoleId {
    RoleId::new(value).expect("valid Slack role id")
}

fn schema_id(value: &str) -> SchemaId {
    SchemaId::new(value).expect("valid Slack schema id")
}

fn setup_check_id(value: &str) -> SetupCheckId {
    SetupCheckId::new(value).expect("valid Slack setup check id")
}

fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("valid Slack tool id")
}

fn trigger_id(value: &str) -> TriggerId {
    TriggerId::new(value).expect("valid Slack trigger id")
}

fn ui_form_id(value: &str) -> UiFormId {
    UiFormId::new(value).expect("valid Slack UI form id")
}
