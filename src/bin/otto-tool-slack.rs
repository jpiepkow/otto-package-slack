use futures_util::{SinkExt, StreamExt};
use otto_extension_sdk::protocol::{
    ContentBlock, HandshakeParams, HandshakeResult, HealthResult, METHOD_HANDSHAKE, METHOD_HEALTH,
    METHOD_REGISTRATIONS_GET, METHOD_SETUP_CALL, METHOD_SETUP_CHECKS_RUN,
    METHOD_SETUP_FORM_CONFIGURATION, METHOD_SHUTDOWN, METHOD_TOOLS_INVOKE,
    METHOD_TRIGGER_FORM_CONFIGURATION, METHOD_TRIGGERS_EVENT, METHOD_TRIGGERS_SUBSCRIBE,
    METHOD_TRIGGERS_UNSUBSCRIBE, RegistrationsResult, SetupCallParams, SetupCallResult,
    SetupCallSpec, SetupCheckRunParams, SetupCheckRunResult, SetupFormConfigurationParams,
    SetupFormConfigurationResult, ShutdownResult, ToolInvokeParams, ToolInvokeResult,
    TriggerEventEnvelope, TriggerEventNotification, TriggerFormConfigurationParams,
    TriggerFormConfigurationResult, TriggerSubscribeParams, TriggerSubscribeResult,
    TriggerUnsubscribeParams, TriggerUnsubscribeResult,
};
use otto_extension_sdk::rpc::framing::{read_rpc_frame, write_rpc_frame};
use otto_tool_slack::{
    DISPLAY_NAME, FAKE_MODE_ARG, PACKAGE_ID, SETUP_READY, TOOL_ADD_REACTION,
    TOOL_LIST_CONVERSATIONS, TOOL_LIST_USERS, TOOL_OPEN_DM, TOOL_READ_CHANNEL, TOOL_READ_DMS,
    TOOL_READ_THREAD, TOOL_SEARCH_MESSAGES, TOOL_SEND_MESSAGE, TRIGGER_MESSAGE, invoke_fake_tool,
    registrations,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::Mutex,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

const SLACK_API_BASE: &str = "https://slack.com/api";

/// Default attribution footer template. Self-contained so it works without any
/// extra configuration. The optional `{Name}` token (if added to a custom
/// template) is replaced with the configured approver name (or a generic label).
const DEFAULT_ATTRIBUTION_TEMPLATE: &str = "— Reviewed & approved to send via Otto";

/// Fallback approver label used when attribution is enabled without a name.
const DEFAULT_ATTRIBUTION_NAME: &str = "an approver";

/// Model-facing result text for a successful, approved `send_message`.
///
/// Every destructive Slack tool reports the approved grant path with the same
/// wording, so a reader of either result sees one phrasing rather than having to
/// infer that a reaction was gated the same way a send was (BUG-007).
const SEND_MESSAGE_APPROVED_SUMMARY: &str = "Sent Slack message through approved send grant.";

/// Maximum reaction summaries surfaced per message in read output.
///
/// Slack allows dozens of distinct reactions on one message. Reads stay bounded
/// the same way message bodies and counts are, and report the overflow instead
/// of growing without limit.
const MAX_MESSAGE_REACTIONS: usize = 8;

/// Maximum characters kept from a Slack emoji name in read output.
const MAX_REACTION_NAME_CHARS: usize = 96;

/// Maximum reacted messages named in the model-facing read text.
const MAX_TEXT_REACTION_MESSAGES: usize = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Clone)]
struct Runtime {
    client: reqwest::Client,
    fake_mode: bool,
    subscriptions: Arc<Mutex<HashMap<String, CancellationToken>>>,
    stdout: SharedStdout,
}

type SharedStdout = Arc<Mutex<tokio::io::Stdout>>;
type RuntimeResult<T> = Result<T, RuntimeError>;

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

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("otto-tool-slack error: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run() -> Result<(), String> {
    let runtime = Runtime {
        client: reqwest::Client::new(),
        fake_mode: fake_mode_enabled(),
        subscriptions: Arc::new(Mutex::new(HashMap::new())),
        stdout: Arc::new(Mutex::new(tokio::io::stdout())),
    };
    let mut stdin = tokio::io::stdin();

    loop {
        let frame = read_rpc_frame(&mut stdin, 1024 * 1024)
            .await
            .map_err(|_| "Slack JSON-RPC frame read failed".to_owned())?;
        let request = serde_json::from_value::<JsonRpcRequest>(frame)
            .map_err(|_| "Slack JSON-RPC request was malformed".to_owned())?;
        let should_shutdown = request.method == METHOD_SHUTDOWN;
        let response = runtime.handle_request(request).await;
        write_shared_response(&runtime.stdout, &response).await?;
        if should_shutdown {
            runtime.cancel_all_subscriptions().await;
            break;
        }
    }

    Ok(())
}

impl Runtime {
    async fn handle_request(&self, request: JsonRpcRequest) -> Value {
        if request.jsonrpc != "2.0" {
            return response(
                request.id,
                Err(RuntimeError::new("unsupported_json_rpc_version")),
            );
        }

        let result = match request.method.as_str() {
            METHOD_HANDSHAKE => handshake(request.params),
            METHOD_HEALTH => Ok(json!(HealthResult {
                healthy: true,
                status: "ok".to_owned(),
                reason: None,
            })),
            METHOD_SETUP_CHECKS_RUN => self.setup_check(request.params).await,
            METHOD_SETUP_FORM_CONFIGURATION => self.setup_form_configuration(request.params),
            METHOD_SETUP_CALL => self.setup_call(request.params).await,
            METHOD_REGISTRATIONS_GET => Ok(json!(RegistrationsResult {
                registrations: registrations(),
            })),
            METHOD_TOOLS_INVOKE => self.invoke_tool(request.params).await,
            METHOD_TRIGGER_FORM_CONFIGURATION => self.trigger_form_configuration(request.params),
            METHOD_TRIGGERS_SUBSCRIBE => self.subscribe(request.params).await,
            METHOD_TRIGGERS_UNSUBSCRIBE => self.unsubscribe(request.params).await,
            METHOD_SHUTDOWN => Ok(json!(ShutdownResult { accepted: true })),
            _ => Err(RuntimeError::new("unknown_slack_runtime_method")),
        };
        response(request.id, result)
    }

    async fn setup_check(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<SetupCheckRunParams>(params)?;
        if params.setup_check_id.as_str() != SETUP_READY {
            return Err(RuntimeError::new("unknown_slack_setup_check"));
        }

        Ok(json!(SetupCheckRunResult {
            setup_check_id: params.setup_check_id,
            ok: true,
            message: Some(if self.fake_mode {
                "Slack fake runtime ready".to_owned()
            } else {
                "Slack runtime ready. Configure a bot or user connection plus Socket Mode app token.".to_owned()
            }),
            details: json!({
                "ready": true,
                "fake_mode": self.fake_mode,
                "credential_ref": if self.fake_mode { "cred.slack.fake" } else { "credential.configured" },
                "workspace_ref": if self.fake_mode { "workspace.fake" } else { "workspace.slack" },
                "channel_refs": if self.fake_mode {
                    json!(["channel.prod-targeting-alerts"])
                } else {
                    json!([])
                },
                "configured_channel_refs": if self.fake_mode {
                    json!(["channel.prod-targeting-alerts"])
                } else {
                    json!([])
                },
                "auth_identity": ["bot", "user"],
                "identity_rule": "exactly_one_web_api_identity",
                "socket_mode": true,
                "supported_modes": ["read", "trigger", "send"],
                "supported_tools": [
                    TOOL_READ_THREAD,
                    TOOL_READ_CHANNEL,
                    TOOL_READ_DMS,
                    TOOL_LIST_CONVERSATIONS,
                    TOOL_LIST_USERS,
                    TOOL_OPEN_DM,
                    TOOL_SEARCH_MESSAGES,
                    TOOL_ADD_REACTION,
                    TOOL_SEND_MESSAGE
                ],
                "limits": {
                    "max_thread_messages": 50,
                    "max_message_chars": 4000,
                    "max_body_chars": 2000
                },
                "redaction": {
                    "message_content": "bounded",
                    "credential_refs": "opaque"
                },
                "warnings": []
            }),
        }))
    }

    fn setup_form_configuration(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<SetupFormConfigurationParams>(params)?;
        let workspace_ref = format!(
            "workspace.slack.{}",
            sanitize_ref(&params.connection.alias_prefix)
        );
        let form = json!({
            "form_id": "slack_setup",
            "schema_ref": "schema.default.slack.setup_details",
            "title": "Slack connection",
            "description": "Configure one Slack identity for Web API calls. Add an app-level token only when this connection should receive Slack triggers.",
            "fields": [
                {
                    "name": "auth_identity",
                    "label": "Identity",
                    "kind": "select",
                    "required": true,
                    "default": "bot",
                    "options": ["bot", "user"],
                    "description": "Choose whether this connection uses a Slack bot token or a user token. A connection uses exactly one identity."
                },
                {
                    "name": "identity_token",
                    "label": "Bot or user token",
                    "kind": "password",
                    "required": true,
                    "x-otto-credential-target": "identity_token_ref",
                    "validation_call": "validate_identity_token",
                    "description": "Slack Web API token for the selected identity. Use xoxb- for bot connections or xoxp-/xoxc-style user tokens for personal connections."
                },
                {
                    "name": "socket_app_token",
                    "label": "Socket Mode app token",
                    "kind": "password",
                    "required": false,
                    "x-otto-credential-target": "socket_app_token_ref",
                    "validation_call": "validate_socket_app_token",
                    "description": "Optional Slack app-level xapp- token. When omitted, read/send tools still work but Slack triggers are unavailable."
                },
                {
                    "name": "workspace_ref",
                    "label": "Workspace reference",
                    "kind": "text",
                    "required": true,
                    "default": workspace_ref,
                    "max_length": 96,
                    "description": "Stable local label for this Slack workspace. It is used in audit records and redacted tool output, not sent to Slack."
                },
                {
                    "name": "max_thread_messages",
                    "label": "Max thread messages",
                    "kind": "number",
                    "required": true,
                    "minimum": 1,
                    "maximum": 50,
                    "default": 20,
                    "description": "Maximum number of messages Otto may read from a single Slack thread."
                },
                {
                    "name": "max_message_chars",
                    "label": "Max message chars",
                    "kind": "number",
                    "required": true,
                    "minimum": 100,
                    "maximum": 4000,
                    "default": 1200,
                    "description": "Maximum characters retained from each Slack message before tool output is bounded and redacted."
                },
                {
                    "name": "max_body_chars",
                    "label": "Max send body chars",
                    "kind": "number",
                    "required": true,
                    "minimum": 100,
                    "maximum": 2000,
                    "default": 1200,
                    "description": "Maximum characters Otto may send after approval."
                },
                {
                    "name": "attribution_enabled",
                    "label": "Append attribution footer",
                    "kind": "boolean",
                    "required": false,
                    "default": false,
                    "description": "When on, outgoing Slack messages (sends and thread replies) carry a transparency footer naming the human approver. This is a trust/transparency label only — it does not change draft-by-default or approval gating."
                },
                {
                    "name": "attribution_name",
                    "label": "Approver name",
                    "kind": "text",
                    "required": false,
                    "max_length": 96,
                    "validation_call": "preview_attribution",
                    "description": "Name shown in the attribution footer (substituted for {Name}). Leave blank to use a generic label."
                },
                {
                    "name": "attribution_template",
                    "label": "Attribution template",
                    "kind": "text",
                    "required": false,
                    "max_length": 200,
                    "default": DEFAULT_ATTRIBUTION_TEMPLATE,
                    "validation_call": "preview_attribution",
                    "description": "Footer template appended to outgoing messages. Works as-is; optionally add {Name} where the approver name should appear (set Approver name above). Preview the rendered footer below."
                }
            ],
            "fixture": {
                "auth_identity": "bot",
                "workspace_ref": workspace_ref,
                "max_thread_messages": 20,
                "max_message_chars": 1200,
                "max_body_chars": 1200,
                "attribution_enabled": false,
                "attribution_template": DEFAULT_ATTRIBUTION_TEMPLATE
            }
        });
        Ok(json!(SetupFormConfigurationResult {
            form,
            calls: vec![
                SetupCallSpec {
                    id: "validate_identity_token".to_owned(),
                    kind: "validation".to_owned(),
                    display_name: "Validate Slack identity token".to_owned(),
                    description: Some("Validates the selected Slack Web API token and reports workspace identity.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: true,
                },
                SetupCallSpec {
                    id: "validate_socket_app_token".to_owned(),
                    kind: "validation".to_owned(),
                    display_name: "Validate Slack Socket Mode token".to_owned(),
                    description: Some("Validates the optional app-level xapp- token used for trigger subscriptions.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: false,
                },
                SetupCallSpec {
                    id: "list_channels".to_owned(),
                    kind: "options".to_owned(),
                    display_name: "List Slack channels".to_owned(),
                    description: Some("Returns Slack channels visible to the validated token.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: false,
                },
                SetupCallSpec {
                    id: "list_users".to_owned(),
                    kind: "options".to_owned(),
                    display_name: "List Slack users".to_owned(),
                    description: Some("Returns Slack users visible to the validated token.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: false,
                },
                SetupCallSpec {
                    id: "connection_capabilities".to_owned(),
                    kind: "capabilities".to_owned(),
                    display_name: "Resolve Slack capabilities".to_owned(),
                    description: Some("Reports which Slack tools and triggers are available from the current setup.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: false,
                },
                SetupCallSpec {
                    id: "preview_attribution".to_owned(),
                    kind: "validation".to_owned(),
                    display_name: "Preview attribution footer".to_owned(),
                    description: Some("Renders a live example of the attribution footer that will be appended to outgoing messages.".to_owned()),
                    input_schema: None,
                    output_schema: None,
                    blocks_continue: false,
                },
            ],
        }))
    }

    fn trigger_form_configuration(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<TriggerFormConfigurationParams>(params)?;
        if params.trigger_id.as_str() != TRIGGER_MESSAGE {
            return Ok(json!(TriggerFormConfigurationResult {
                form: json!({}),
                calls: Vec::new(),
            }));
        }

        let form = json!({
            "form_id": "slack_message_trigger",
            "title": "Slack message trigger",
            "description": "Choose the Slack channels this job should listen to. Otto stores channel IDs in trigger scope; message events outside this list are ignored.",
            "fields": [
                {
                    "name": "trigger_channel_ids",
                    "label": "Trigger channels",
                    "kind": "string_list",
                    "required": true,
                    "options_call": "list_channels",
                    "description": "One Slack channel ID per line. Use Load options to pick visible channels such as #otto-slack-e2e."
                }
            ]
        });

        Ok(json!(TriggerFormConfigurationResult {
            form,
            calls: vec![SetupCallSpec {
                id: "list_channels".to_owned(),
                kind: "options".to_owned(),
                display_name: "List Slack channels".to_owned(),
                description: Some(
                    "Returns Slack channels visible to the validated token.".to_owned()
                ),
                input_schema: None,
                output_schema: None,
                blocks_continue: false,
            }],
        }))
    }

    async fn setup_call(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<SetupCallParams>(params)?;
        match params.call_id.as_str() {
            "validate_identity_token" => self.validate_identity_token(&params).await,
            "validate_socket_app_token" => self.validate_socket_app_token(&params).await,
            "list_channels" => self.list_setup_channels(&params).await,
            "list_users" => self.list_setup_users(&params).await,
            "preview_attribution" => Ok(json!(preview_attribution_result(&params))),
            "connection_capabilities" => Ok(json!(SetupCallResult {
                status: "ok".to_owned(),
                message: Some("Slack connection capabilities resolved.".to_owned()),
                output: json!({
                    "tools_enabled": setup_secret(&params, "identity_token").is_some()
                        || config_has_secret_ref(&params.config, "identity_token_ref"),
                    "triggers_enabled": setup_secret(&params, "socket_app_token").is_some()
                        || config_has_secret_ref(&params.config, "socket_app_token_ref"),
                    "tools": [
                        TOOL_READ_THREAD,
                        TOOL_READ_CHANNEL,
                        TOOL_READ_DMS,
                        TOOL_LIST_CONVERSATIONS,
                        TOOL_LIST_USERS,
                        TOOL_OPEN_DM,
                        TOOL_SEARCH_MESSAGES,
                        TOOL_ADD_REACTION,
                        TOOL_SEND_MESSAGE
                    ],
                    "triggers": [TRIGGER_MESSAGE]
                }),
            })),
            _ => Err(RuntimeError::new("unknown_slack_setup_call")),
        }
    }

    async fn validate_identity_token(&self, params: &SetupCallParams) -> RuntimeResult<Value> {
        let Some(token) = setup_secret(params, "identity_token") else {
            return Ok(json!(SetupCallResult {
                status: "missing".to_owned(),
                message: Some("Enter a Slack bot or user token before validating.".to_owned()),
                output: json!({}),
            }));
        };
        let identity = params
            .config
            .get("auth_identity")
            .and_then(Value::as_str)
            .unwrap_or("bot");
        if identity == "bot" && token.starts_with("xoxp-") {
            return Ok(json!(SetupCallResult {
                status: "invalid".to_owned(),
                message: Some(
                    "Bot connections must use a bot token, usually starting with xoxb-.".to_owned()
                ),
                output: json!({ "identity": identity }),
            }));
        }
        if identity == "user" && token.starts_with("xoxb-") {
            return Ok(json!(SetupCallResult {
                status: "invalid".to_owned(),
                message: Some(
                    "User connections must use a user token, usually starting with xoxp- or xoxc-."
                        .to_owned()
                ),
                output: json!({ "identity": identity }),
            }));
        }
        if self.fake_mode {
            return Ok(json!(SetupCallResult {
                status: "valid".to_owned(),
                message: Some("Slack fake token accepted.".to_owned()),
                output: json!({
                    "team_id": "TFAKE",
                    "team": "Fake Slack",
                    "user_id": if identity == "bot" { "BFAKE" } else { "UFAKE" },
                    "identity": identity
                }),
            }));
        }
        let auth = self.slack_api(&token, "auth.test", &[]).await?;
        Ok(json!(SetupCallResult {
            status: "valid".to_owned(),
            message: Some("Slack token validated.".to_owned()),
            output: auth,
        }))
    }

    async fn validate_socket_app_token(&self, params: &SetupCallParams) -> RuntimeResult<Value> {
        let Some(token) = setup_secret(params, "socket_app_token") else {
            return Ok(json!(SetupCallResult {
                status: "not_configured".to_owned(),
                message: Some("No Socket Mode token provided. Slack triggers will be unavailable for this connection.".to_owned()),
                output: json!({ "triggers_enabled": false }),
            }));
        };
        if self.fake_mode {
            return Ok(json!(SetupCallResult {
                status: "valid".to_owned(),
                message: Some("Slack fake Socket Mode token accepted.".to_owned()),
                output: json!({ "triggers_enabled": true }),
            }));
        }
        let open = self.slack_api(&token, "apps.connections.open", &[]).await?;
        Ok(json!(SetupCallResult {
            status: "valid".to_owned(),
            message: Some("Slack Socket Mode token validated.".to_owned()),
            output: json!({
                "triggers_enabled": true,
                "socket_mode_url_available": open.get("url").and_then(Value::as_str).is_some()
            }),
        }))
    }

    async fn list_setup_channels(&self, params: &SetupCallParams) -> RuntimeResult<Value> {
        let Some(token) = setup_secret(params, "identity_token") else {
            return Ok(json!(SetupCallResult {
                status: "missing".to_owned(),
                message: Some("Validate an identity token before loading channels.".to_owned()),
                output: json!({ "options": [] }),
            }));
        };
        if self.fake_mode {
            return Ok(json!(SetupCallResult {
                status: "ok".to_owned(),
                message: Some("Loaded fake Slack channels.".to_owned()),
                output: json!({
                    "options": [
                        { "value": "CFAKEINC", "label": "#incidents", "description": "Fake incident channel" },
                        { "value": "CFAKEENG", "label": "#engineering", "description": "Fake engineering channel" }
                    ]
                }),
            }));
        }
        let response = self
            .slack_api(
                &token,
                "conversations.list",
                &[
                    ("types", "public_channel,private_channel,im,mpim"),
                    ("limit", "200"),
                    ("exclude_archived", "true"),
                ],
            )
            .await?;
        Ok(json!(SetupCallResult {
            status: "ok".to_owned(),
            message: Some("Loaded Slack channels visible to this token.".to_owned()),
            output: json!({
                "options": setup_channel_options(&response),
                "response_metadata": response.get("response_metadata").cloned().unwrap_or(Value::Null)
            }),
        }))
    }

    async fn list_setup_users(&self, params: &SetupCallParams) -> RuntimeResult<Value> {
        let Some(token) = setup_secret(params, "identity_token") else {
            return Ok(json!(SetupCallResult {
                status: "missing".to_owned(),
                message: Some("Validate an identity token before loading users.".to_owned()),
                output: json!({ "options": [] }),
            }));
        };
        if self.fake_mode {
            return Ok(json!(SetupCallResult {
                status: "ok".to_owned(),
                message: Some("Loaded fake Slack users.".to_owned()),
                output: json!({
                    "options": [
                        { "value": "UFAKE1", "label": "Jordan", "description": "Fake Slack user" },
                        { "value": "UFAKE2", "label": "Otto Test", "description": "Fake Slack user" }
                    ]
                }),
            }));
        }
        let response = self
            .slack_api(&token, "users.list", &[("limit", "200")])
            .await?;
        Ok(json!(SetupCallResult {
            status: "ok".to_owned(),
            message: Some("Loaded Slack users visible to this token.".to_owned()),
            output: json!({
                "options": setup_user_options(&response),
                "response_metadata": response.get("response_metadata").cloned().unwrap_or(Value::Null)
            }),
        }))
    }

    async fn invoke_tool(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<ToolInvokeParams>(params)?;
        if self.fake_mode {
            return Ok(json!(invoke_fake_tool(&params)?));
        }
        let result = match params.tool_id.as_str() {
            TOOL_READ_THREAD => self.read_thread(&params).await?,
            TOOL_READ_CHANNEL => self.read_channel(&params).await?,
            TOOL_READ_DMS => self.read_recent_dms(&params).await?,
            TOOL_LIST_CONVERSATIONS => self.list_conversations(&params).await?,
            TOOL_LIST_USERS => self.list_users(&params).await?,
            TOOL_OPEN_DM => self.open_dm_tool(&params).await?,
            TOOL_SEARCH_MESSAGES => self.search_messages(&params).await?,
            TOOL_ADD_REACTION => self.add_reaction(&params).await?,
            TOOL_SEND_MESSAGE => self.send_message(&params).await?,
            _ => return Err(RuntimeError::new("unknown_slack_tool")),
        };
        Ok(json!(result))
    }

    async fn subscribe(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<TriggerSubscribeParams>(params)?;
        if params.trigger_id.as_str() != TRIGGER_MESSAGE {
            return Err(RuntimeError::new("unknown_slack_trigger"));
        }
        let config = SlackConnectionConfig::from_scope(&params.scope)?;
        let app_token = config
            .socket_app_token
            .clone()
            .ok_or_else(|| RuntimeError::new("slack_socket_app_token_missing"))?;
        let subscription_id = format!("slack-socket-{}", params.trigger_id.as_str());
        let cancel = CancellationToken::new();
        self.subscriptions
            .lock()
            .await
            .insert(subscription_id.clone(), cancel.clone());

        let runtime = self.clone();
        let subscription_id_for_task = subscription_id.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime
                .run_socket_mode_subscription(
                    params,
                    config,
                    app_token,
                    subscription_id_for_task,
                    cancel,
                )
                .await
            {
                eprintln!("slack socket subscription ended: {error}");
            }
        });

        Ok(json!(TriggerSubscribeResult {
            subscription_id,
            accepted: true,
        }))
    }

    async fn unsubscribe(&self, params: Option<Value>) -> RuntimeResult<Value> {
        let params = decode_params::<TriggerUnsubscribeParams>(params)?;
        if let Some(cancel) = self
            .subscriptions
            .lock()
            .await
            .remove(&params.subscription_id)
        {
            cancel.cancel();
        }
        Ok(json!(TriggerUnsubscribeResult { accepted: true }))
    }

    async fn cancel_all_subscriptions(&self) {
        let subscriptions = self
            .subscriptions
            .lock()
            .await
            .drain()
            .map(|(_, cancel)| cancel)
            .collect::<Vec<_>>();
        for cancel in subscriptions {
            cancel.cancel();
        }
    }

    async fn run_socket_mode_subscription(
        &self,
        params: TriggerSubscribeParams,
        config: SlackConnectionConfig,
        app_token: String,
        subscription_id: String,
        cancel: CancellationToken,
    ) -> RuntimeResult<()> {
        let open = self
            .slack_api(&app_token, "apps.connections.open", &[])
            .await?;
        let url = open
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::owned("slack_socket_url_missing"))?;
        let (mut ws, _) = connect_async(url).await.map_err(|error| {
            RuntimeError::owned(format!("slack_socket_connect_failed: {error}"))
        })?;

        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                frame = ws.next() => {
                    let Some(frame) = frame else {
                        break;
                    };
                    let frame = frame
                        .map_err(|error| RuntimeError::owned(format!("slack_socket_read_failed: {error}")))?;
                    let Message::Text(text) = frame else {
                        continue;
                    };
                    let envelope = serde_json::from_str::<Value>(&text)
                        .map_err(|_| RuntimeError::new("slack_socket_envelope_invalid"))?;
                    if let Some(envelope_id) = envelope.get("envelope_id").and_then(Value::as_str) {
                        let ack = json!({ "envelope_id": envelope_id });
                        ws.send(Message::Text(ack.to_string().into()))
                            .await
                            .map_err(|error| RuntimeError::owned(format!("slack_socket_ack_failed: {error}")))?;
                    }
                    if let Some(notification) = slack_notification_from_envelope(
                        &params,
                        &config,
                        &subscription_id,
                        &envelope,
                    ) {
                        write_trigger_notification(&self.stdout, &notification).await?;
                    }
                }
            }
        }
        let _ = ws.close(None).await;
        Ok(())
    }

    async fn read_thread(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let channel_arg = string_arg(
            &params.arguments,
            &["channel_id", "channel_ref", "channel"],
            None,
        )
        .ok_or_else(|| RuntimeError::new("slack_channel_required"))?;
        let channel = match self.resolve_channel_arg(&config, &channel_arg).await {
            Ok(channel) => channel,
            Err(channel_error) => {
                let explicit_user = string_arg(
                    &params.arguments,
                    &["user_id", "user", "recipient_id", "recipient"],
                    None,
                );
                let user_arg = explicit_user.as_deref().unwrap_or(channel_arg.as_str());
                match self.resolve_user_arg(&config, user_arg).await {
                    Ok(user) => self.open_dm(&config.identity_token, &user).await?,
                    Err(_) => return Err(channel_error),
                }
            }
        };
        let thread_ts = string_arg(
            &params.arguments,
            &["thread_ts", "thread_ref", "ts", "message_ref", "message_id"],
            None,
        )
        .ok_or_else(|| RuntimeError::new("slack_thread_ts_required"))?;
        let thread_ts = deref_ts(&thread_ts);
        let limit = int_arg(
            &params.arguments,
            "max_messages",
            config.max_thread_messages,
        )
        .clamp(1, 50);
        let limit_string = limit.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "conversations.replies",
                &[
                    ("channel", channel.as_str()),
                    ("ts", thread_ts.as_str()),
                    ("limit", limit_string.as_str()),
                ],
            )
            .await?;
        let messages = bounded_messages(&response, config.max_message_chars);
        let summary = format!(
            "Read {} Slack thread messages.{}",
            messages.len(),
            reaction_text_suffix(&messages)
        );
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_ref": channel_ref(&channel),
                "thread_ref": thread_ref(&thread_ts),
                "messages": messages,
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn read_channel(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let channel_arg = string_arg(
            &params.arguments,
            &["channel_id", "channel_ref", "channel"],
            None,
        );
        let limit = int_arg(&params.arguments, "limit", config.max_thread_messages).clamp(1, 50);
        let Some(channel_arg) = channel_arg else {
            return self.read_configured_channels(params, &config, limit).await;
        };
        let channel = self.resolve_channel_arg(&config, &channel_arg).await?;
        let limit_string = limit.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "conversations.history",
                &[
                    ("channel", channel.as_str()),
                    ("limit", limit_string.as_str()),
                ],
            )
            .await?;
        let messages = bounded_messages(&response, config.max_message_chars);
        let summary = format!(
            "Read {} recent Slack channel messages.{}",
            messages.len(),
            reaction_text_suffix(&messages)
        );
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_ref": channel_ref(&channel),
                "messages": messages,
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn read_configured_channels(
        &self,
        params: &ToolInvokeParams,
        config: &SlackConnectionConfig,
        limit: usize,
    ) -> RuntimeResult<ToolInvokeResult> {
        let max_channels = int_arg(
            &params.arguments,
            "max_channels",
            int_arg(&params.arguments, "max_dm_channels", 5),
        )
        .clamp(1, 20);
        let messages_per_channel = int_arg(
            &params.arguments,
            "messages_per_channel",
            int_arg(&params.arguments, "messages_per_dm", limit),
        )
        .clamp(1, 20);
        let channels = self.list_readable_channel_ids(config, max_channels).await?;
        if channels.is_empty() {
            return Err(RuntimeError::new("slack_channel_required"));
        }

        let mut channel_reads = Vec::new();
        let mut message_count = 0usize;
        let messages_per_channel_string = messages_per_channel.to_string();
        for channel in channels {
            let history = self
                .slack_api(
                    &config.identity_token,
                    "conversations.history",
                    &[
                        ("channel", channel.as_str()),
                        ("limit", messages_per_channel_string.as_str()),
                    ],
                )
                .await?;
            let messages = bounded_messages(&history, config.max_message_chars);
            message_count += messages.len();
            channel_reads.push(json!({
                "channel_id": channel,
                "channel_ref": channel_ref(&channel),
                "messages": messages
            }));
        }

        let summary = format!(
            "Read {message_count} recent Slack channel messages across {} channels.",
            channel_reads.len()
        );
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "channels": channel_reads,
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn read_recent_dms(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let limit = int_arg(&params.arguments, "limit", 10).clamp(1, 20);
        let channel = match string_arg(&params.arguments, &["channel_id", "dm_channel_id"], None) {
            Some(channel) => self.resolve_channel_arg(&config, &channel).await?,
            None => {
                let Some(user) = string_arg(&params.arguments, &["user_id", "user"], None) else {
                    return self.read_recent_dm_overview(params, &config, limit).await;
                };
                self.open_dm(&config.identity_token, &user).await?
            }
        };
        let limit_string = limit.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "conversations.history",
                &[
                    ("channel", channel.as_str()),
                    ("limit", limit_string.as_str()),
                ],
            )
            .await?;
        let messages = bounded_messages(&response, config.max_message_chars);
        let summary = format!(
            "Read {} recent Slack DM messages.{}",
            messages.len(),
            reaction_text_suffix(&messages)
        );
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_ref": channel_ref(&channel),
                "messages": messages,
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn read_recent_dm_overview(
        &self,
        params: &ToolInvokeParams,
        config: &SlackConnectionConfig,
        limit: usize,
    ) -> RuntimeResult<ToolInvokeResult> {
        let max_dm_channels = int_arg(&params.arguments, "max_dm_channels", 10).clamp(1, 20);
        let messages_per_dm = int_arg(&params.arguments, "messages_per_dm", limit).clamp(1, 20);
        let max_dm_channels_string = max_dm_channels.to_string();
        let channels_response = self
            .slack_api(
                &config.identity_token,
                "conversations.list",
                &[
                    ("types", "im,mpim"),
                    ("limit", max_dm_channels_string.as_str()),
                    ("exclude_archived", "true"),
                ],
            )
            .await?;
        let channels = channels_response
            .get("channels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut conversations = Vec::new();
        let mut message_count = 0usize;
        for channel_value in channels {
            let Some(channel_id) = channel_value.get("id").and_then(Value::as_str) else {
                continue;
            };
            let user_id = channel_value.get("user").and_then(Value::as_str);
            let messages_per_dm_string = messages_per_dm.to_string();
            let history = self
                .slack_api(
                    &config.identity_token,
                    "conversations.history",
                    &[
                        ("channel", channel_id),
                        ("limit", messages_per_dm_string.as_str()),
                    ],
                )
                .await?;
            let messages = bounded_messages(&history, config.max_message_chars);
            message_count += messages.len();
            conversations.push(json!({
                "channel_id": channel_id,
                "channel_ref": channel_ref(channel_id),
                "user_id": user_id,
                "messages": messages
            }));
        }
        let summary = format!(
            "Read {message_count} recent Slack DM messages across {} conversations.",
            conversations.len()
        );
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "dm_conversations": conversations,
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn list_readable_channel_ids(
        &self,
        config: &SlackConnectionConfig,
        max_channels: usize,
    ) -> RuntimeResult<Vec<String>> {
        let limit_string = max_channels.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "conversations.list",
                &[
                    ("types", "public_channel,private_channel"),
                    ("limit", limit_string.as_str()),
                    ("exclude_archived", "true"),
                ],
            )
            .await?;
        Ok(response
            .get("channels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|channel| channel.get("id").and_then(Value::as_str))
            .take(max_channels)
            .map(str::to_owned)
            .collect())
    }

    async fn list_conversations(
        &self,
        params: &ToolInvokeParams,
    ) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let types = string_arg(
            &params.arguments,
            &["types"],
            Some("public_channel,private_channel,im,mpim"),
        )
        .expect("default present");
        let limit = int_arg(&params.arguments, "limit", 100).clamp(1, 200);
        let limit_string = limit.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "conversations.list",
                &[
                    ("types", types.as_str()),
                    ("limit", limit_string.as_str()),
                    ("exclude_archived", "true"),
                ],
            )
            .await?;
        let channels = response
            .get("channels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let summary = format!("Listed {} Slack conversations.", channels.len());
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "conversations": channels,
                "truncated": false
            }),
        ))
    }

    async fn list_users(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let query = string_arg(&params.arguments, &["query", "user", "name"], None)
            .map(|value| user_name_key(&value))
            .filter(|value| !value.is_empty());
        let limit = int_arg(&params.arguments, "limit", 100).clamp(1, 200);
        let mut cursor = String::new();
        let mut users = Vec::new();
        let mut truncated = false;

        for _ in 0..10 {
            let limit_string = limit.to_string();
            let mut form = vec![("limit", limit_string.as_str())];
            if !cursor.is_empty() {
                form.push(("cursor", cursor.as_str()));
            }
            let response = self
                .slack_api(&config.identity_token, "users.list", &form)
                .await?;
            let members = response
                .get("members")
                .and_then(Value::as_array)
                .ok_or_else(|| RuntimeError::new("slack_user_list_missing"))?;
            for member in members {
                if member.get("deleted").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(_id) = member.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(query) = &query
                    && !user_candidate_partially_matches(member, query)
                {
                    continue;
                }
                users.push(slack_user_summary(member));
                if users.len() >= limit {
                    truncated = true;
                    break;
                }
            }
            if users.len() >= limit {
                break;
            }
            cursor = response
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if cursor.is_empty() {
                break;
            }
        }

        let summary = format!("Listed {} Slack users.", users.len());
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "users": users,
                "query": query,
                "truncated": truncated
            }),
        ))
    }

    async fn open_dm_tool(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let user = string_arg(&params.arguments, &["user_id", "user"], None)
            .ok_or_else(|| RuntimeError::new("slack_user_required"))?;
        let user = self.resolve_user_arg(&config, &user).await?;
        let channel = self.open_dm(&config.identity_token, &user).await?;
        Ok(ok_result(
            "Opened Slack DM channel.",
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_id": channel,
                "channel_ref": channel_ref(&channel),
                "user_id": user
            }),
        ))
    }

    async fn send_message(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "send")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let channel_arg = string_arg(
            &params.arguments,
            &["channel_id", "channel_ref", "channel"],
            None,
        );
        let user_arg = string_arg(
            &params.arguments,
            &["user_id", "user", "recipient_id", "recipient"],
            None,
        );
        let channel = match (channel_arg.as_deref(), user_arg.as_deref()) {
            (Some(channel_arg), user_arg) => {
                match self.resolve_channel_arg(&config, channel_arg).await {
                    Ok(channel) => channel,
                    Err(channel_error) => {
                        let user_target = user_arg.unwrap_or(channel_arg);
                        match self.resolve_user_arg(&config, user_target).await {
                            Ok(user) => self.open_dm(&config.identity_token, &user).await?,
                            Err(user_error) => {
                                return Err(RuntimeError::owned(format!(
                                    "{channel_error}; {user_error}; send_message needs channel_id for a Slack channel or user_id for a Slack DM. Use list_users first when the user gives a human name."
                                )));
                            }
                        }
                    }
                }
            }
            (None, Some(user_arg)) => {
                let user = self.resolve_user_arg(&config, user_arg).await?;
                self.open_dm(&config.identity_token, &user).await?
            }
            (None, None) => return Err(RuntimeError::new("slack_channel_or_user_required")),
        };
        let raw_text = string_arg(&params.arguments, &["text", "body", "message"], None)
            .ok_or_else(|| RuntimeError::new("slack_message_body_required"))?;
        // Append the attribution footer (when enabled) BEFORE bounding so the
        // footer is part of the bounded body. This single site also covers
        // thread replies, which push thread_ts onto the same form below.
        let text = apply_attribution(&raw_text, &config);
        let text = bound(&text, config.max_body_chars);
        let mut form = vec![("channel", channel.as_str()), ("text", text.as_str())];
        let thread_ts = string_arg(&params.arguments, &["thread_ts", "thread_ref"], None);
        if let Some(thread_ts) = &thread_ts {
            form.push(("thread_ts", thread_ts.as_str()));
        }
        let response = self
            .slack_api(&config.identity_token, "chat.postMessage", &form)
            .await?;
        Ok(ok_result(
            SEND_MESSAGE_APPROVED_SUMMARY,
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_ref": channel_ref(&channel),
                "message_ref": message_ref(response.get("ts").and_then(Value::as_str).unwrap_or("sent")),
                "sent": true,
                "slack": response
            }),
        ))
    }

    async fn search_messages(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "read")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        search_requires_user_token(&config.identity_token)?;
        let query = string_arg(&params.arguments, &["query"], None)
            .ok_or_else(|| RuntimeError::new("slack_search_query_required"))?;
        let query = bound(&query, 256);
        let count = int_arg(&params.arguments, "count", 20).clamp(1, 100);
        let count_string = count.to_string();
        let response = self
            .slack_api(
                &config.identity_token,
                "search.messages",
                &[("query", query.as_str()), ("count", count_string.as_str())],
            )
            .await?;
        let matches = bounded_search_matches(&response, config.max_message_chars);
        let summary = format!("Found {} Slack search matches.", matches.len());
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "query": query,
                "matches": matches,
                "match_count": matches.len(),
                "truncated": false,
                "redactions_applied": ["message_content"]
            }),
        ))
    }

    async fn add_reaction(&self, params: &ToolInvokeParams) -> RuntimeResult<ToolInvokeResult> {
        ensure_mode(params, "send")?;
        let config = SlackConnectionConfig::from_scope(&params.package_scope)?;
        let channel_arg = string_arg(
            &params.arguments,
            &["channel", "channel_id", "channel_ref"],
            None,
        )
        .ok_or_else(|| RuntimeError::new("slack_channel_required"))?;
        let channel = self.resolve_channel_arg(&config, &channel_arg).await?;
        let timestamp = string_arg(
            &params.arguments,
            &["timestamp", "ts", "thread_ts", "message_ref"],
            None,
        )
        .ok_or_else(|| RuntimeError::new("slack_reaction_timestamp_required"))?;
        let timestamp = deref_ts(&timestamp);
        let name_arg = string_arg(&params.arguments, &["name", "emoji"], None)
            .ok_or_else(|| RuntimeError::new("slack_reaction_name_required"))?;
        let name = name_arg.trim().trim_matches(':').to_owned();
        if name.is_empty() {
            return Err(RuntimeError::new("slack_reaction_name_required"));
        }
        let response = self
            .slack_api(
                &config.identity_token,
                "reactions.add",
                &[
                    ("channel", channel.as_str()),
                    ("timestamp", timestamp.as_str()),
                    ("name", name.as_str()),
                ],
            )
            .await?;
        let summary = add_reaction_summary(&name);
        Ok(ok_result(
            &summary,
            json!({
                "workspace_ref": config.workspace_ref,
                "channel_ref": channel_ref(&channel),
                "timestamp": timestamp,
                "name": name,
                "ok": true,
                "slack": response
            }),
        ))
    }

    async fn open_dm(&self, token: &str, user: &str) -> RuntimeResult<String> {
        let response = self
            .slack_api(token, "conversations.open", &[("users", user)])
            .await?;
        response
            .get("channel")
            .and_then(|channel| channel.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| RuntimeError::new("slack_dm_channel_missing"))
    }

    async fn resolve_channel_arg(
        &self,
        config: &SlackConnectionConfig,
        channel_arg: &str,
    ) -> RuntimeResult<String> {
        let normalized = normalize_channel_arg(channel_arg);
        if looks_like_slack_channel_id(&normalized) {
            return Ok(canonical_channel_value(&normalized));
        }
        self.lookup_channel_by_name(config, &normalized).await
    }

    async fn resolve_user_arg(
        &self,
        config: &SlackConnectionConfig,
        user_arg: &str,
    ) -> RuntimeResult<String> {
        let normalized = normalize_user_arg(user_arg);
        if looks_like_slack_user_id(&normalized) {
            return Ok(canonical_user_value(&normalized));
        }
        self.lookup_user_by_name(config, &normalized).await
    }

    async fn lookup_channel_by_name(
        &self,
        config: &SlackConnectionConfig,
        channel_name: &str,
    ) -> RuntimeResult<String> {
        let requested = normalize_channel_name(channel_name);
        let mut cursor = String::new();
        for _ in 0..10 {
            let mut form = vec![
                ("types", "public_channel,private_channel,mpim"),
                ("limit", "200"),
                ("exclude_archived", "true"),
            ];
            if !cursor.is_empty() {
                form.push(("cursor", cursor.as_str()));
            }
            let response = self
                .slack_api(&config.identity_token, "conversations.list", &form)
                .await?;
            let channels = response
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(|| RuntimeError::new("slack_channel_list_missing"))?;
            for channel in channels {
                let Some(id) = channel.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name_matches = channel
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| normalize_channel_name(name) == requested);
                if name_matches || normalize_channel_name(id) == requested {
                    return Ok(id.to_owned());
                }
            }
            cursor = response
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if cursor.is_empty() {
                break;
            }
        }
        Err(RuntimeError::owned(format!(
            "slack_channel_name_not_found:{channel_name}"
        )))
    }

    async fn lookup_user_by_name(
        &self,
        config: &SlackConnectionConfig,
        user_name: &str,
    ) -> RuntimeResult<String> {
        let requested = user_name_key(user_name);
        let mut cursor = String::new();
        let mut partial_matches = Vec::new();
        for _ in 0..10 {
            let mut form = vec![("limit", "200")];
            if !cursor.is_empty() {
                form.push(("cursor", cursor.as_str()));
            }
            let response = self
                .slack_api(&config.identity_token, "users.list", &form)
                .await?;
            let members = response
                .get("members")
                .and_then(Value::as_array)
                .ok_or_else(|| RuntimeError::new("slack_user_list_missing"))?;
            for member in members {
                if member.get("deleted").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let Some(id) = member.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if user_candidate_matches(member, &requested) {
                    return Ok(id.to_owned());
                }
                if user_candidate_partially_matches(member, &requested) {
                    partial_matches.push(id.to_owned());
                }
            }
            cursor = response
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if cursor.is_empty() {
                break;
            }
        }
        partial_matches.sort();
        partial_matches.dedup();
        if partial_matches.len() == 1 {
            return partial_matches.into_iter().next().ok_or_else(|| {
                RuntimeError::owned(format!("slack_user_name_not_found:{user_name}"))
            });
        }
        if partial_matches.len() > 1 {
            return Err(RuntimeError::owned(format!(
                "slack_user_name_ambiguous:{user_name}:matches={}",
                partial_matches.join(",")
            )));
        }
        Err(RuntimeError::owned(format!(
            "slack_user_name_not_found:{user_name}"
        )))
    }

    async fn slack_api(
        &self,
        token: &str,
        method: &str,
        form: &[(&str, &str)],
    ) -> RuntimeResult<Value> {
        let url = format!("{SLACK_API_BASE}/{method}");
        // Slack Web API methods must be sent as application/x-www-form-urlencoded.
        // search.messages in particular (a user-token method) silently ignores a
        // JSON body, so every method goes through .form() with the same arg slice.
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .form(form)
            .send()
            .await
            .map_err(|error| RuntimeError::owned(format!("slack_http_error: {error}")))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| RuntimeError::owned(format!("slack_response_invalid: {error}")))?;
        if !status.is_success() {
            return Err(RuntimeError::owned(format!(
                "slack_http_status_{status}{}",
                retry_after
                    .as_deref()
                    .map(|retry| format!("_retry_after_{retry}s"))
                    .unwrap_or_default()
            )));
        }
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(value)
        } else {
            Err(RuntimeError::owned(slack_api_error_message(method, &value)))
        }
    }
}

#[derive(Debug, Clone)]
struct SlackConnectionConfig {
    identity_token: String,
    socket_app_token: Option<String>,
    workspace_ref: String,
    max_thread_messages: usize,
    max_message_chars: usize,
    max_body_chars: usize,
    attribution_enabled: bool,
    attribution_name: Option<String>,
    attribution_template: Option<String>,
    /// Per-trigger channel targets carried in the job's trigger scope
    /// (`source.scope.trigger_channel_ids`, merged authoritatively at runtime).
    /// Replaces the removed connection-level channel allowlist; the Socket Mode
    /// filter fails closed when this is empty (no workspace firehose).
    trigger_channel_ids: Vec<String>,
}

impl SlackConnectionConfig {
    fn from_scope(scope: &Value) -> RuntimeResult<Self> {
        let auth_identity = string_scope(scope, "auth_identity")
            .unwrap_or_else(|| infer_identity(scope).unwrap_or_else(|| "bot".to_owned()));
        if !matches!(auth_identity.as_str(), "bot" | "user") {
            return Err(RuntimeError::new("slack_auth_identity_must_be_bot_or_user"));
        }
        if auth_identity == "bot" && scope_has_any(scope, &["user_token_ref", "user_token"]) {
            return Err(RuntimeError::new(
                "slack_bot_connection_must_not_include_user_token",
            ));
        }
        if auth_identity == "user" && scope_has_any(scope, &["bot_token_ref", "bot_token"]) {
            return Err(RuntimeError::new(
                "slack_user_connection_must_not_include_bot_token",
            ));
        }
        if scope_has_any(scope, &["bot_token_ref", "bot_token"])
            && scope_has_any(scope, &["user_token_ref", "user_token"])
        {
            return Err(RuntimeError::new(
                "slack_connection_must_not_have_bot_and_user_tokens",
            ));
        }
        let identity_token = secret(scope, "identity_token")
            .or_else(|| secret(scope, "bot_token"))
            .or_else(|| secret(scope, "user_token"))
            .ok_or_else(|| RuntimeError::new("slack_identity_token_missing"))?;
        if auth_identity == "bot" && identity_token.starts_with("xoxp-") {
            return Err(RuntimeError::new(
                "slack_bot_connection_received_user_token",
            ));
        }
        if auth_identity == "user" && identity_token.starts_with("xoxb-") {
            return Err(RuntimeError::new(
                "slack_user_connection_received_bot_token",
            ));
        }
        let socket_app_token = secret(scope, "socket_app_token");

        Ok(Self {
            identity_token,
            socket_app_token,
            workspace_ref: string_scope(scope, "workspace_ref")
                .unwrap_or_else(|| "workspace.slack".to_owned()),
            max_thread_messages: usize_scope(scope, "max_thread_messages").unwrap_or(20),
            max_message_chars: usize_scope(scope, "max_message_chars").unwrap_or(1200),
            max_body_chars: usize_scope(scope, "max_body_chars").unwrap_or(1200),
            attribution_enabled: bool_scope(scope, "attribution_enabled").unwrap_or(false),
            attribution_name: string_scope(scope, "attribution_name")
                .filter(|name| !name.trim().is_empty()),
            attribution_template: string_scope(scope, "attribution_template")
                .filter(|template| !template.trim().is_empty()),
            trigger_channel_ids: string_list_scope(scope, "trigger_channel_ids")
                .unwrap_or_default(),
        })
    }
}

fn handshake(params: Option<Value>) -> RuntimeResult<Value> {
    let params = decode_params::<HandshakeParams>(params)?;
    if params.package_id.as_str() != PACKAGE_ID {
        return Err(RuntimeError::new("handshake_package_id_mismatch"));
    }

    Ok(json!(HandshakeResult {
        protocol_version: params.protocol_version,
        package_id: params.package_id,
        display_name: DISPLAY_NAME.to_owned(),
    }))
}

fn slack_notification_from_envelope(
    params: &TriggerSubscribeParams,
    config: &SlackConnectionConfig,
    subscription_id: &str,
    envelope: &Value,
) -> Option<TriggerEventNotification> {
    let payload = envelope.get("payload")?;
    let event = payload.get("event")?;
    if event.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    if event.get("subtype").and_then(Value::as_str).is_some()
        || event.get("bot_id").and_then(Value::as_str).is_some()
    {
        return None;
    }
    let channel = event.get("channel").and_then(Value::as_str)?;
    // Per-trigger channel targeting (SC-3): fail closed when no target is set
    // (empty list -> match nothing; never a workspace firehose) and drop any
    // message whose channel is not in the trigger scope's trigger_channel_ids.
    if config.trigger_channel_ids.is_empty()
        || !config.trigger_channel_ids.iter().any(|c| c == channel)
    {
        return None;
    }
    let ts = event.get("ts").and_then(Value::as_str)?;
    let team = payload
        .get("team_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("team").and_then(Value::as_str))
        .unwrap_or("slack");
    let text = event.get("text").and_then(Value::as_str).unwrap_or("");
    let user = event
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let thread = event.get("thread_ts").and_then(Value::as_str).unwrap_or(ts);
    let dedupe_key = format!(
        "slack/{}/{}/{}",
        sanitize_ref(team),
        sanitize_ref(channel),
        sanitize_ref(ts)
    );
    let item_scope = format!("slack/{}/{}", sanitize_ref(team), sanitize_ref(channel));
    let cursor = Value::String(ts.to_owned());
    Some(TriggerEventNotification {
        subscription_id: Some(subscription_id.to_owned()),
        trigger_id: params.trigger_id.clone(),
        event: TriggerEventEnvelope {
            payload: json!({
                "workspace_ref": config.workspace_ref,
                "team_id": team,
                "channel_id": channel,
                "channel_ref": channel_ref(channel),
                "thread_ts": thread,
                "thread_ref": thread_ref(thread),
                "message_ref": message_ref(ts),
                "author_ref": actor_ref(user),
                "text": bound(text, config.max_message_chars),
                "dedupe_key": dedupe_key,
                "item_scope": item_scope,
                "cursor": ts,
                "message_kind": if channel.starts_with('D') { "dm" } else { "channel" },
                "bounded_task_context": format!(
                    "Slack message in {channel} from {user}: {}",
                    bound(text, 800)
                )
            }),
            dedupe_key,
            cursor,
            item_scope: Some(item_scope),
            occurred_at: None,
        },
    })
}

/// Model-facing result text for a successful, approved `add_reaction`.
///
/// Structurally mirrors [`SEND_MESSAGE_APPROVED_SUMMARY`]: the acting verb
/// differs, the "through approved send grant" phrase does not. Both destructive
/// Slack tools are `requires_approval = true` against `cap.default.slack.send`,
/// and both now say so (BUG-007).
fn add_reaction_summary(name: &str) -> String {
    format!("Added Slack reaction :{name}: through approved send grant.")
}

fn bounded_messages(response: &Value, max_chars: usize) -> Vec<Value> {
    response
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|message| {
                    let ts = message
                        .get("ts")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let user = message
                        .get("user")
                        .and_then(Value::as_str)
                        .or_else(|| message.get("bot_id").and_then(Value::as_str))
                        .unwrap_or("unknown");
                    let body = message.get("text").and_then(Value::as_str).unwrap_or("");
                    let mut entry = json!({
                        "message_ref": message_ref(ts),
                        "author_ref": actor_ref(user),
                        "sent_at": Value::Null,
                        "body": bound(body, max_chars),
                        "body_chars": body.chars().count().min(max_chars),
                        "body_digest": format!("slack:{}", sanitize_ref(ts))
                    });
                    if let Some((reactions, truncated)) = bounded_reactions(message) {
                        let object = entry
                            .as_object_mut()
                            .expect("bounded message entry is a JSON object");
                        object.insert("reactions".to_owned(), reactions);
                        object.insert("reactions_truncated".to_owned(), Value::Bool(truncated));
                    }
                    entry
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Bounded, secret-free reaction summary for one Slack message.
///
/// Slack attaches `reactions: [{name, count, users: [...]}]` to a message in
/// `conversations.history` / `conversations.replies` output only when the
/// message has reactions AND the token carries `reactions:read`. Returns
/// `None` when the field is absent or empty, so an unreacted message's read
/// output is unchanged.
///
/// The summary keeps `{name, count}` only. The per-reaction `users` member list
/// is NEVER echoed — it is reduced to a `users_truncated` marker — because a
/// read that verifies a reaction must not quietly become a channel-membership
/// disclosure. Entries are capped at [`MAX_MESSAGE_REACTIONS`], mirroring how
/// the package already bounds message bodies and counts, and the overflow is
/// reported rather than silently dropped (BUG-007).
fn bounded_reactions(message: &Value) -> Option<(Value, bool)> {
    let reactions = message.get("reactions").and_then(Value::as_array)?;
    if reactions.is_empty() {
        return None;
    }
    let summarized = reactions
        .iter()
        .take(MAX_MESSAGE_REACTIONS)
        .map(|reaction| {
            let name = reaction
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let count = reaction.get("count").and_then(Value::as_u64).unwrap_or(0);
            let users_truncated = reaction
                .get("users")
                .and_then(Value::as_array)
                .is_some_and(|users| !users.is_empty());
            json!({
                "name": bound(name, MAX_REACTION_NAME_CHARS),
                "count": count,
                "users_truncated": users_truncated
            })
        })
        .collect::<Vec<_>>();
    Some((
        Value::Array(summarized),
        reactions.len() > MAX_MESSAGE_REACTIONS,
    ))
}

/// Bounded model-facing sentence describing the reactions present in a read.
///
/// Operates on entries already shaped by [`bounded_messages`], so it can only
/// ever see the capped, `users`-free summary — the raw Slack payload is out of
/// reach by construction. Returns an empty string when nothing is reacted, so an
/// unreacted read's text is byte-identical to before. Otherwise it names at most
/// [`MAX_TEXT_REACTION_MESSAGES`] reacted messages with the same `{name, count}`
/// pairs carried in `structured_content`, so a reaction side effect is
/// verifiable from either channel (BUG-007).
fn reaction_text_suffix(messages: &[Value]) -> String {
    let reacted = messages
        .iter()
        .filter(|message| {
            message
                .get("reactions")
                .and_then(Value::as_array)
                .is_some_and(|reactions| !reactions.is_empty())
        })
        .collect::<Vec<_>>();
    if reacted.is_empty() {
        return String::new();
    }
    let described = reacted
        .iter()
        .take(MAX_TEXT_REACTION_MESSAGES)
        .map(|message| {
            let reference = message
                .get("message_ref")
                .and_then(Value::as_str)
                .unwrap_or("message.unknown");
            let pairs = message
                .get("reactions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|reaction| {
                    let name = reaction
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let count = reaction.get("count").and_then(Value::as_u64).unwrap_or(0);
                    format!(":{name}: x{count}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{reference} {pairs}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    let overflow = if reacted.len() > MAX_TEXT_REACTION_MESSAGES {
        "; …"
    } else {
        ""
    };
    format!(" {} with reactions: {described}{overflow}.", reacted.len())
}

fn bounded_search_matches(response: &Value, max_chars: usize) -> Vec<Value> {
    response
        .get("messages")
        .and_then(|messages| messages.get("matches"))
        .and_then(Value::as_array)
        .map(|matches| {
            matches
                .iter()
                .map(|matched| {
                    let ts = matched
                        .get("ts")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let user = matched
                        .get("user")
                        .and_then(Value::as_str)
                        .or_else(|| matched.get("username").and_then(Value::as_str))
                        .or_else(|| matched.get("bot_id").and_then(Value::as_str))
                        .unwrap_or("unknown");
                    let channel = matched
                        .get("channel")
                        .and_then(|channel| channel.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let body = matched.get("text").and_then(Value::as_str).unwrap_or("");
                    json!({
                        "message_ref": message_ref(ts),
                        "channel_ref": channel_ref(channel),
                        "author_ref": actor_ref(user),
                        "sent_at": Value::Null,
                        "body": bound(body, max_chars),
                        "body_chars": body.chars().count().min(max_chars),
                        "body_digest": format!("slack:{}", sanitize_ref(ts))
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ensure_mode(params: &ToolInvokeParams, expected: &str) -> RuntimeResult<()> {
    let scope_mode = params
        .package_scope
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or(expected);
    if scope_mode != expected {
        return Err(RuntimeError::new("slack_tool_mode_mismatch"));
    }
    Ok(())
}

fn decode_params<T>(params: Option<Value>) -> RuntimeResult<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| RuntimeError::new("invalid_slack_runtime_params"))
}

#[allow(clippy::needless_pass_by_value)]
fn response(id: Value, result: RuntimeResult<Value>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": error.message
            }
        }),
    }
}

async fn write_shared_response(writer: &SharedStdout, response: &Value) -> Result<(), String> {
    let mut writer = writer.lock().await;
    write_response(&mut *writer, response).await
}

async fn write_response<W>(writer: &mut W, response: &Value) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
{
    write_rpc_frame(writer, response)
        .await
        .map_err(|_| "Slack JSON-RPC frame write failed".to_owned())?;
    writer
        .flush()
        .await
        .map_err(|_| "Slack JSON-RPC flush failed".to_owned())
}

async fn write_trigger_notification(
    writer: &SharedStdout,
    notification: &TriggerEventNotification,
) -> RuntimeResult<()> {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": METHOD_TRIGGERS_EVENT,
        "params": notification
    });
    write_shared_response(writer, &frame)
        .await
        .map_err(RuntimeError::owned)
}

fn fake_mode_enabled() -> bool {
    std::env::args().any(|arg| arg == FAKE_MODE_ARG)
}

fn setup_secret(params: &SetupCallParams, name: &str) -> Option<String> {
    params
        .secrets
        .get(name)
        .or_else(|| params.secrets.get(&format!("{name}_ref")))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn config_has_secret_ref(config: &Value, field: &str) -> bool {
    config.get(field).is_some_and(|value| !value.is_null())
}

fn setup_channel_options(response: &Value) -> Vec<Value> {
    response
        .get("channels")
        .and_then(Value::as_array)
        .map(|channels| {
            channels
                .iter()
                .filter_map(|channel| {
                    let id = channel.get("id").and_then(Value::as_str)?;
                    let name = channel
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| channel.get("user").and_then(Value::as_str))
                        .unwrap_or(id);
                    let prefix = if id.starts_with('D') { "dm:" } else { "#" };
                    Some(json!({
                        "value": id,
                        "label": format!("{prefix}{name}"),
                        "description": channel_description(channel),
                        "metadata": channel,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn slack_api_error_message(method: &str, value: &Value) -> String {
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let mut parts = vec![
        format!("slack_api_error:{error}"),
        format!("method={method}"),
    ];
    if let Some(needed) = value.get("needed").and_then(Value::as_str) {
        parts.push(format!("needed_scope={needed}"));
    }
    if let Some(provided) = value.get("provided").and_then(Value::as_str) {
        parts.push(format!("provided_scopes={provided}"));
    }
    parts.join(":")
}

fn setup_user_options(response: &Value) -> Vec<Value> {
    response
        .get("members")
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|member| {
                    if member.get("deleted").and_then(Value::as_bool) == Some(true) {
                        return None;
                    }
                    let id = member.get("id").and_then(Value::as_str)?;
                    let profile = member.get("profile").unwrap_or(&Value::Null);
                    let display_name = profile
                        .get("display_name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .or_else(|| profile.get("real_name").and_then(Value::as_str))
                        .or_else(|| member.get("name").and_then(Value::as_str))
                        .unwrap_or(id);
                    Some(json!({
                        "value": id,
                        "label": display_name,
                        "description": member.get("name").and_then(Value::as_str).unwrap_or(id),
                        "metadata": member,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn slack_user_summary(member: &Value) -> Value {
    let id = member
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let profile = member.get("profile").unwrap_or(&Value::Null);
    let name = member.get("name").and_then(Value::as_str).unwrap_or(id);
    let real_name = member
        .get("real_name")
        .and_then(Value::as_str)
        .or_else(|| profile.get("real_name").and_then(Value::as_str))
        .unwrap_or("");
    let display_name = profile
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(name);
    json!({
        "user_id": id,
        "actor_ref": actor_ref(id),
        "name": name,
        "real_name": real_name,
        "display_name": display_name,
        "is_bot": member.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
        "is_app_user": member.get("is_app_user").and_then(Value::as_bool).unwrap_or(false)
    })
}

fn channel_description(channel: &Value) -> String {
    let mut parts = Vec::new();
    if channel.get("is_private").and_then(Value::as_bool) == Some(true) {
        parts.push("private");
    }
    if channel.get("is_im").and_then(Value::as_bool) == Some(true) {
        parts.push("dm");
    }
    if channel.get("is_mpim").and_then(Value::as_bool) == Some(true) {
        parts.push("group dm");
    }
    if parts.is_empty() {
        "channel".to_owned()
    } else {
        parts.join(", ")
    }
}

fn secret(scope: &Value, name: &str) -> Option<String> {
    scope
        .get("_otto_secrets")
        .and_then(Value::as_object)
        .and_then(|secrets| {
            secrets
                .get(name)
                .or_else(|| secrets.get(&format!("{name}_ref")))
                .or_else(|| secrets.get(&format!("{name}_credential_ref")))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn string_scope(scope: &Value, key: &str) -> Option<String> {
    scope.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn usize_scope(scope: &Value, key: &str) -> Option<usize> {
    scope
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn bool_scope(scope: &Value, key: &str) -> Option<bool> {
    scope.get(key).and_then(Value::as_bool)
}

/// Read a `Vec<String>` from a scope array key (e.g. `trigger_channel_ids`).
/// Non-string array entries are skipped; a missing/non-array key yields None.
fn string_list_scope(scope: &Value, key: &str) -> Option<Vec<String>> {
    scope.get(key).and_then(Value::as_array).map(|values| {
        values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    })
}

/// Render the attribution footer from an optional template + name. `{Name}` is
/// substituted by `name` (falling back to a generic label when empty/None).
/// Shared by the runtime send path and the setup preview call.
fn render_attribution_footer(template: Option<&str>, name: Option<&str>) -> String {
    let template = template
        .map(str::trim)
        .filter(|template| !template.is_empty())
        .unwrap_or(DEFAULT_ATTRIBUTION_TEMPLATE);
    let name = name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_ATTRIBUTION_NAME);
    template.replace("{Name}", name)
}

/// Build the live attribution-footer preview for the setup form. Reads the
/// submitted (non-secret) `attribution_name`/`attribution_template` values and
/// returns the rendered footer as the call message so the UI can surface it as
/// help text under the field. Treated as enabled so the operator always sees an
/// example, independent of the toggle state.
fn preview_attribution_result(params: &SetupCallParams) -> SetupCallResult {
    let name = params
        .config
        .get("attribution_name")
        .and_then(Value::as_str);
    let template = params
        .config
        .get("attribution_template")
        .and_then(Value::as_str);
    let footer = render_attribution_footer(template, name);
    SetupCallResult {
        status: "ok".to_owned(),
        message: Some(format!("Example footer: {footer}")),
        output: json!({ "footer": footer }),
    }
}

/// Append the attribution footer to outgoing text when enabled; otherwise
/// return the text unchanged. A single call site covers send_message and its
/// thread replies (same send path).
fn apply_attribution(text: &str, config: &SlackConnectionConfig) -> String {
    if !config.attribution_enabled {
        return text.to_owned();
    }
    let footer = render_attribution_footer(
        config.attribution_template.as_deref(),
        config.attribution_name.as_deref(),
    );
    format!("{text}\n{footer}")
}

fn string_arg(arguments: &Value, keys: &[&str], fallback: Option<&str>) -> Option<String> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .or(fallback)
        .map(str::to_owned)
}

fn int_arg(arguments: &Value, key: &str, fallback: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(fallback)
}

fn scope_has_any(scope: &Value, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        scope.get(*key).is_some()
            || scope
                .get("_otto_secrets")
                .and_then(Value::as_object)
                .is_some_and(|secrets| secrets.contains_key(*key))
    })
}

fn infer_identity(scope: &Value) -> Option<String> {
    if scope_has_any(scope, &["bot_token", "bot_token_ref"]) {
        Some("bot".to_owned())
    } else if scope_has_any(scope, &["user_token", "user_token_ref"]) {
        Some("user".to_owned())
    } else {
        None
    }
}

fn bound(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn normalize_channel_arg(value: &str) -> String {
    let trimmed = value.trim();
    let without_hash = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let without_ref = without_hash
        .strip_prefix("channel.")
        .unwrap_or(without_hash);
    without_ref.trim().to_owned()
}

fn normalize_channel_name(value: &str) -> String {
    normalize_channel_arg(value).to_ascii_lowercase()
}

fn looks_like_slack_channel_id(value: &str) -> bool {
    let normalized = normalize_channel_arg(value);
    let mut chars = normalized.chars();
    matches!(
        chars.next().map(|character| character.to_ascii_uppercase()),
        Some('C' | 'G' | 'D')
    ) && normalized.len() >= 9
        && chars.all(|character| character.is_ascii_alphanumeric())
}

fn canonical_channel_value(value: &str) -> String {
    let normalized = normalize_channel_arg(value);
    if looks_like_slack_channel_id(&normalized) {
        normalized.to_ascii_uppercase()
    } else {
        normalized
    }
}

fn normalize_user_arg(value: &str) -> String {
    let trimmed = value.trim();
    let mention = trimmed
        .strip_prefix("<@")
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or(trimmed);
    let without_actor = mention.strip_prefix("actor.").unwrap_or(mention);
    let without_at = without_actor.strip_prefix('@').unwrap_or(without_actor);
    without_at.trim().to_owned()
}

fn looks_like_slack_user_id(value: &str) -> bool {
    let normalized = normalize_user_arg(value);
    let mut chars = normalized.chars();
    matches!(
        chars.next().map(|character| character.to_ascii_uppercase()),
        Some('U' | 'W')
    ) && normalized.len() >= 9
        && chars.all(|character| character.is_ascii_alphanumeric())
}

fn canonical_user_value(value: &str) -> String {
    let normalized = normalize_user_arg(value);
    if looks_like_slack_user_id(&normalized) {
        normalized.to_ascii_uppercase()
    } else {
        normalized
    }
}

fn user_name_key(value: &str) -> String {
    normalize_user_arg(value)
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn user_candidate_matches(member: &Value, requested: &str) -> bool {
    if requested.is_empty() {
        return false;
    }
    let profile = member.get("profile").unwrap_or(&Value::Null);
    [
        member.get("id").and_then(Value::as_str),
        member.get("name").and_then(Value::as_str),
        member.get("real_name").and_then(Value::as_str),
        profile.get("display_name").and_then(Value::as_str),
        profile.get("real_name").and_then(Value::as_str),
        profile.get("real_name_normalized").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(|candidate| user_name_key(candidate) == requested)
}

fn user_candidate_partially_matches(member: &Value, requested: &str) -> bool {
    if requested.is_empty() {
        return false;
    }
    let profile = member.get("profile").unwrap_or(&Value::Null);
    [
        member.get("id").and_then(Value::as_str),
        member.get("name").and_then(Value::as_str),
        member.get("real_name").and_then(Value::as_str),
        profile.get("display_name").and_then(Value::as_str),
        profile.get("real_name").and_then(Value::as_str),
        profile.get("real_name_normalized").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .map(user_name_key)
    .any(|candidate| candidate == requested || candidate.contains(requested))
}

fn channel_ref(value: &str) -> String {
    format!("channel.{}", sanitize_ref(value))
}

fn thread_ref(value: &str) -> String {
    format!("thread.{}", sanitize_ref(value))
}

fn message_ref(value: &str) -> String {
    format!("message.{}", sanitize_ref(value))
}

/// Strip a leading `thread.`, `message.`, or `ts.` ref prefix to recover the
/// raw Slack timestamp. A raw ts (or any value without a known prefix) passes
/// through unchanged. `sanitize_ref` is lossless for a Slack ts, so stripping
/// the prefix recovers the original value the tool emitted.
fn deref_ts(value: &str) -> String {
    for prefix in ["thread.", "message.", "ts."] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return rest.to_owned();
        }
    }
    value.to_owned()
}

/// `search.messages` is a Slack user-token-only method. A bot token (`xoxb-`)
/// returns an opaque `invalid_arguments`, so reject it up front with a stable,
/// matchable code. The guard keys specifically on the `xoxb-` prefix, so a
/// valid user token (`xoxp-`) passes through unchanged.
fn search_requires_user_token(identity_token: &str) -> Result<(), RuntimeError> {
    if identity_token.starts_with("xoxb-") {
        return Err(RuntimeError::new("slack_search_requires_user_token"));
    }
    Ok(())
}

fn actor_ref(value: &str) -> String {
    format!("actor.{}", sanitize_ref(value))
}

fn sanitize_ref(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['.', '-', '_'])
        .to_owned();
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized
    }
}

#[derive(Debug, Clone)]
struct RuntimeError {
    message: String,
}

impl RuntimeError {
    fn new(message: &'static str) -> Self {
        Self {
            message: message.to_owned(),
        }
    }

    fn owned(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<otto_tool_slack::ToolRuntimeError> for RuntimeError {
    fn from(error: otto_tool_slack::ToolRuntimeError) -> Self {
        Self::new(error.message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_extension_sdk::extension_ids::TriggerId;

    fn scoped_secrets(identity_token: &str) -> Value {
        json!({
            "auth_identity": "bot",
            "_otto_secrets": {
                "identity_token": identity_token,
                "socket_app_token": "xapp-test-token"
            }
        })
    }

    #[test]
    fn connection_config_requires_exactly_one_bot_or_user_identity() {
        let bot = SlackConnectionConfig::from_scope(&scoped_secrets("xoxb-test-token"))
            .expect("bot identity is accepted");
        assert_eq!(bot.identity_token, "xoxb-test-token");

        let bot_with_user_token = json!({
            "auth_identity": "bot",
            "user_token_ref": "cred.slack.user",
            "_otto_secrets": {
                "user_token": "xoxp-test-token",
                "socket_app_token": "xapp-test-token"
            }
        });
        assert_eq!(
            SlackConnectionConfig::from_scope(&bot_with_user_token)
                .expect_err("bot connection rejects user token field")
                .message,
            "slack_bot_connection_must_not_include_user_token"
        );

        let mixed = json!({
            "auth_identity": "user",
            "bot_token_ref": "cred.slack.bot",
            "user_token_ref": "cred.slack.user",
            "_otto_secrets": {
                "bot_token": "xoxb-test-token",
                "user_token": "xoxp-test-token",
                "socket_app_token": "xapp-test-token"
            }
        });
        assert_eq!(
            SlackConnectionConfig::from_scope(&mixed)
                .expect_err("mixed bot/user token refs are rejected")
                .message,
            "slack_user_connection_must_not_include_bot_token"
        );

        let wrong_prefix = json!({
            "auth_identity": "user",
            "_otto_secrets": {
                "identity_token": "xoxb-test-token",
                "socket_app_token": "xapp-test-token"
            }
        });
        assert_eq!(
            SlackConnectionConfig::from_scope(&wrong_prefix)
                .expect_err("user connection rejects bot token prefix")
                .message,
            "slack_user_connection_received_bot_token"
        );
    }

    #[test]
    fn from_scope_succeeds_without_allowlist_fields() {
        // SC-2: a scope with NO allowlist keys (only token + workspace) must
        // deserialize cleanly now that connection authority is "token reach".
        let scope = json!({
            "auth_identity": "user",
            "workspace_ref": "workspace.test",
            "_otto_secrets": {
                "identity_token": "xoxp-test"
            }
        });
        let config = SlackConnectionConfig::from_scope(&scope)
            .expect("from_scope succeeds when allowlist fields are absent");
        assert_eq!(config.identity_token, "xoxp-test");
        assert_eq!(config.workspace_ref, "workspace.test");
    }

    #[test]
    fn attribution_footer_appended() {
        // SLACK-ATTR-01: when attribution is enabled, the configured footer is
        // appended to outgoing text with {Name} substituted by attribution_name.
        // Use an explicit template containing {Name} so the substitution proof
        // is independent of the default template (which is self-contained).
        let scope = json!({
            "auth_identity": "user",
            "workspace_ref": "workspace.test",
            "attribution_enabled": true,
            "attribution_name": "Jordan Piepkow",
            "attribution_template": "— Reviewed & approved by {Name} · via Otto",
            "_otto_secrets": {
                "identity_token": "xoxp-test"
            }
        });
        let config = SlackConnectionConfig::from_scope(&scope)
            .expect("from_scope succeeds with attribution fields");
        assert!(config.attribution_enabled);
        assert_eq!(config.attribution_name.as_deref(), Some("Jordan Piepkow"));

        let rendered = apply_attribution("Investigation complete.", &config);
        assert!(
            rendered.starts_with("Investigation complete.\n"),
            "original text is preserved before the footer: {rendered}"
        );
        assert!(
            rendered.contains("Reviewed & approved by Jordan Piepkow"),
            "{{Name}} is substituted by attribution_name: {rendered}"
        );
        assert!(
            rendered.contains("via Otto"),
            "template tail is present: {rendered}"
        );
        assert!(
            !rendered.contains("{Name}"),
            "no unsubstituted placeholder remains: {rendered}"
        );
    }

    #[test]
    fn attribution_default_template_is_self_contained() {
        // The default template needs no configuration: with attribution enabled
        // and no name/template set, it renders a complete footer and never leaks
        // an unsubstituted {Name} placeholder.
        let scope = json!({
            "auth_identity": "user",
            "workspace_ref": "workspace.test",
            "attribution_enabled": true,
            "_otto_secrets": {
                "identity_token": "xoxp-test"
            }
        });
        let config = SlackConnectionConfig::from_scope(&scope)
            .expect("from_scope succeeds with attribution enabled and no name");
        let rendered = apply_attribution("Done.", &config);
        assert!(
            rendered.contains("Reviewed & approved to send via Otto"),
            "default self-contained footer is present: {rendered}"
        );
        assert!(
            !rendered.contains("{Name}"),
            "default template carries no placeholder: {rendered}"
        );
    }

    #[test]
    fn attribution_disabled_no_change() {
        // SLACK-ATTR-01: when attribution is disabled (default), outgoing text
        // is returned unchanged.
        let scope = json!({
            "auth_identity": "user",
            "workspace_ref": "workspace.test",
            "_otto_secrets": {
                "identity_token": "xoxp-test"
            }
        });
        let config = SlackConnectionConfig::from_scope(&scope)
            .expect("from_scope succeeds without attribution fields");
        assert!(
            !config.attribution_enabled,
            "attribution defaults to disabled"
        );

        let text = "Just the message.";
        assert_eq!(apply_attribution(text, &config), text);
    }

    #[test]
    fn channel_argument_normalization_handles_names_refs_and_ids() {
        assert_eq!(normalize_channel_arg("#all-otto"), "all-otto");
        assert_eq!(normalize_channel_arg("channel.all-otto"), "all-otto");
        assert_eq!(normalize_channel_name("#All-Otto"), "all-otto");
        assert!(looks_like_slack_channel_id("C0B6UBMFHA7"));
        assert!(looks_like_slack_channel_id("channel.c0b6ubmfha7"));
        assert!(!looks_like_slack_channel_id("all-otto"));
    }

    #[test]
    fn user_argument_normalization_handles_names_refs_mentions_and_ids() {
        assert_eq!(normalize_user_arg("@john"), "john");
        assert_eq!(normalize_user_arg("actor.u0b72hqez09"), "u0b72hqez09");
        assert_eq!(normalize_user_arg("<@U0B72HQEZ09>"), "U0B72HQEZ09");
        assert!(looks_like_slack_user_id("U0B72HQEZ09"));
        assert_eq!(canonical_user_value("actor.u0b72hqez09"), "U0B72HQEZ09");
        assert_eq!(user_name_key("Jordan Piepkow"), "jordanpiepkow");

        let member = json!({
            "id": "U0B72HQEZ09",
            "name": "jpiepkow",
            "profile": {
                "display_name": "Jordan",
                "real_name": "Jordan Piepkow"
            }
        });
        assert!(user_candidate_matches(
            &member,
            &user_name_key("jordan piepkow")
        ));
        assert!(user_candidate_matches(&member, &user_name_key("Jordan")));
        assert!(user_candidate_matches(&member, &user_name_key("jpiepkow")));
        assert!(!user_candidate_matches(
            &member,
            &user_name_key("someone else")
        ));
    }

    #[test]
    fn slack_api_error_message_includes_scope_details() {
        let message = slack_api_error_message(
            "conversations.history",
            &json!({
                "ok": false,
                "error": "missing_scope",
                "needed": "channels:history",
                "provided": "channels:read,users:read"
            }),
        );
        assert_eq!(
            message,
            "slack_api_error:missing_scope:method=conversations.history:needed_scope=channels:history:provided_scopes=channels:read,users:read"
        );
    }

    #[test]
    fn socket_mode_notification_emits_for_message_events() {
        // Connection-level channel/user pinning is removed; the Socket Mode
        // filter targets per-trigger trigger_channel_ids from the trigger scope.
        // This test asserts the remaining envelope-shaping behavior (message-type
        // filter + payload/dedupe construction) is intact for a targeted channel.
        let config = SlackConnectionConfig::from_scope(&json!({
            "auth_identity": "bot",
            "trigger_channel_ids": ["C123"],
            "_otto_secrets": {
                "identity_token": "xoxb-test-token",
                "socket_app_token": "xapp-test-token"
            }
        }))
        .expect("scoped config");
        let params = TriggerSubscribeParams {
            trigger_id: TriggerId::new(TRIGGER_MESSAGE).expect("trigger id"),
            scope: Value::Null,
            cursor: None,
            heartbeat_ms: None,
            poll_interval_ms: None,
        };
        let envelope = json!({
            "envelope_id": "env-1",
            "payload": {
                "team_id": "T123",
                "event": {
                    "type": "message",
                    "channel": "C123",
                    "user": "U123",
                    "ts": "1710000000.000100",
                    "text": "hello from Slack"
                }
            }
        });
        let notification = slack_notification_from_envelope(&params, &config, "sub-1", &envelope)
            .expect("message event emits notification");
        assert_eq!(
            notification.event.payload["bounded_task_context"],
            "Slack message in C123 from U123: hello from Slack"
        );
        assert_eq!(notification.event.payload["channel_ref"], "channel.c123");
        assert_eq!(
            notification.event.dedupe_key,
            "slack/t123/c123/1710000000.000100"
        );

        // Non-message envelopes are still dropped by the type filter.
        let non_message = json!({
            "payload": {
                "team_id": "T123",
                "event": {
                    "type": "reaction_added",
                    "channel": "C123",
                    "user": "U999",
                    "ts": "1710000000.000101"
                }
            }
        });
        assert!(
            slack_notification_from_envelope(&params, &config, "sub-1", &non_message).is_none()
        );

        // Bot/subtype messages, including Otto's own sends, must not retrigger
        // the job and create a feedback loop.
        for event in [
            json!({
                "type": "message",
                "subtype": "bot_message",
                "channel": "C123",
                "bot_id": "B123",
                "ts": "1710000000.000102",
                "text": "bot reply"
            }),
            json!({
                "type": "message",
                "subtype": "message_changed",
                "channel": "C123",
                "user": "U123",
                "ts": "1710000000.000103",
                "text": "edited"
            }),
        ] {
            let envelope = json!({
                "payload": {
                    "team_id": "T123",
                    "event": event
                }
            });
            assert!(
                slack_notification_from_envelope(&params, &config, "sub-1", &envelope).is_none()
            );
        }
    }

    #[test]
    fn socket_mode_trigger_scope_filter() {
        // Plan 48-04 SC-3: the Socket Mode filter targets per-trigger
        // `trigger_channel_ids` carried in the job's trigger scope (NOT a
        // connection allowlist). A message in a targeted channel emits a
        // notification whose dedupe_key/item_scope/cursor VALUES are asserted
        // exactly (offline acceptance, not merely is_some); a non-targeted
        // channel and an empty target both fail closed (no firehose).
        //
        // Shape (sanitize_ref lowercases refs): a `T1`/`C123`/`1700000000.000100`
        // event yields dedupe_key shaped slack/T1/C123/<ts> (asserted lowercased
        // as the emitted concrete string slack/t1/c123/1700000000.000100).
        let params = TriggerSubscribeParams {
            trigger_id: TriggerId::new(TRIGGER_MESSAGE).expect("trigger id"),
            scope: Value::Null,
            cursor: None,
            heartbeat_ms: None,
            poll_interval_ms: None,
        };
        let envelope = |channel: &str| {
            json!({
                "envelope_id": "env-1",
                "payload": {
                    "team_id": "T1",
                    "event": {
                        "type": "message",
                        "channel": channel,
                        "user": "U1",
                        "ts": "1700000000.000100",
                        "text": "hello"
                    }
                }
            })
        };

        // Targeted channel -> Some(notification) with EXACT emitted values.
        let targeted = SlackConnectionConfig::from_scope(&json!({
            "auth_identity": "bot",
            "trigger_channel_ids": ["C123"],
            "_otto_secrets": {
                "identity_token": "xoxb-test-token",
                "socket_app_token": "xapp-test-token"
            }
        }))
        .expect("scoped config");
        let notification =
            slack_notification_from_envelope(&params, &targeted, "sub-1", &envelope("C123"))
                .expect("targeted channel emits notification");
        assert_eq!(
            notification.event.dedupe_key,
            "slack/t1/c123/1700000000.000100"
        );
        assert_eq!(
            notification.event.item_scope.as_deref(),
            Some("slack/t1/c123")
        );
        assert_eq!(
            notification.event.cursor,
            Value::String("1700000000.000100".to_owned())
        );
        // Same values mirrored in the emitted payload.
        assert_eq!(
            notification.event.payload["dedupe_key"],
            "slack/t1/c123/1700000000.000100"
        );
        assert_eq!(notification.event.payload["item_scope"], "slack/t1/c123");
        assert_eq!(notification.event.payload["cursor"], "1700000000.000100");

        // Non-targeted channel -> None.
        assert!(
            slack_notification_from_envelope(&params, &targeted, "sub-1", &envelope("C999"))
                .is_none(),
            "non-targeted channel must be filtered out"
        );

        // Empty trigger_channel_ids -> None (fail-closed; no workspace firehose).
        let empty = SlackConnectionConfig::from_scope(&json!({
            "auth_identity": "bot",
            "_otto_secrets": {
                "identity_token": "xoxb-test-token",
                "socket_app_token": "xapp-test-token"
            }
        }))
        .expect("scoped config");
        assert!(
            slack_notification_from_envelope(&params, &empty, "sub-1", &envelope("C123")).is_none(),
            "empty trigger_channel_ids must fail closed"
        );
    }

    #[test]
    fn deref_ts_strips_thread_prefix() {
        assert_eq!(deref_ts("thread.1710000000.000100"), "1710000000.000100");
    }

    #[test]
    fn deref_ts_strips_message_prefix() {
        assert_eq!(deref_ts("message.1710000000.000100"), "1710000000.000100");
    }

    #[test]
    fn deref_ts_strips_ts_prefix() {
        assert_eq!(deref_ts("ts.1710000000.000100"), "1710000000.000100");
    }

    #[test]
    fn deref_ts_passes_raw_ts_through_unchanged() {
        assert_eq!(deref_ts("1710000000.000100"), "1710000000.000100");
    }

    #[test]
    fn deref_ts_round_trips_thread_and_message_refs() {
        let ts = "1710000000.000100";
        assert_eq!(deref_ts(&thread_ref(ts)), ts);
        assert_eq!(deref_ts(&message_ref(ts)), ts);
    }

    #[test]
    fn add_reaction_summary_reports_the_approved_grant_path_like_send_message() {
        // BUG-007 (1): add_reaction returned ok:true but its result text did not
        // name the approved grant path the way send_message does, so the approval
        // gate was only implied. Both destructive tools now report the same
        // literal phrase. Asserted as literal substrings so a future reword of
        // either tool is caught here rather than in a live UAT transcript.
        let summary = add_reaction_summary("white_check_mark");
        assert_eq!(
            summary,
            "Added Slack reaction :white_check_mark: through approved send grant."
        );
        assert!(
            summary.contains("through approved send grant"),
            "add_reaction names the approved grant path: {summary}"
        );
        assert_eq!(
            SEND_MESSAGE_APPROVED_SUMMARY,
            "Sent Slack message through approved send grant."
        );
        assert!(
            SEND_MESSAGE_APPROVED_SUMMARY.contains("through approved send grant"),
            "send_message keeps the shared approved-grant phrase: {SEND_MESSAGE_APPROVED_SUMMARY}"
        );
    }

    /// A `conversations.history` / `conversations.replies` response shaped the
    /// way Slack returns one when the token carries `reactions:read`: the first
    /// message has reactions (each with a `users` member array), the second has
    /// no `reactions` key at all.
    fn reacted_history_response() -> Value {
        json!({
            "ok": true,
            "messages": [
                {
                    "ts": "1782957738.004499",
                    "user": "U0B72HQEZ09",
                    "text": "Otto acceptance marker.",
                    "reactions": [
                        {
                            "name": "white_check_mark",
                            "count": 2,
                            "users": ["U0B72HQEZ09", "U0B72HQEZ10"]
                        },
                        {
                            "name": "eyes",
                            "count": 1,
                            "users": ["U0B72HQEZ11"]
                        }
                    ]
                },
                {
                    "ts": "1782957739.004500",
                    "user": "U0B72HQEZ09",
                    "text": "A message nobody reacted to."
                }
            ]
        })
    }

    #[test]
    fn read_output_surfaces_bounded_reaction_summary() {
        // BUG-007 (2): a reaction side effect must be verifiable through a
        // SEPARATE read, not only from the mutating call's own response.
        let messages = bounded_messages(&reacted_history_response(), 1200);
        let reactions = messages[0]["reactions"]
            .as_array()
            .expect("a reacted message carries a reaction summary");
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0]["name"], "white_check_mark");
        assert_eq!(reactions[0]["count"], 2);
        assert_eq!(reactions[1]["name"], "eyes");
        assert_eq!(reactions[1]["count"], 1);
        assert_eq!(messages[0]["reactions_truncated"], false);
        // The same pairs are mirrored into the model-facing text so a reaction
        // is verifiable without parsing the structured payload.
        let text = reaction_text_suffix(&messages);
        assert!(
            text.contains("message.1782957738.004499"),
            "reacted message is named in the read text: {text}"
        );
        assert!(
            text.contains(":white_check_mark: x2"),
            "reaction name and count appear in the read text: {text}"
        );
    }

    #[test]
    fn read_output_never_echoes_the_slack_users_array() {
        // Reads stay bounded and secret-free: Slack's per-reaction `users`
        // member list is reduced to a marker, never echoed into read output.
        let messages = bounded_messages(&reacted_history_response(), 1200);
        let serialized = Value::Array(messages.clone()).to_string();
        assert!(
            !serialized.contains("U0B72HQEZ10"),
            "reacting member ids must not reach read output: {serialized}"
        );
        assert!(
            !serialized.contains("\"users\""),
            "the raw Slack users array must not reach read output: {serialized}"
        );
        assert_eq!(messages[0]["reactions"][0]["users_truncated"], true);
        let text = reaction_text_suffix(&messages);
        assert!(
            !text.contains("U0B72HQEZ10"),
            "reacting member ids must not reach the read text: {text}"
        );
    }

    #[test]
    fn read_output_omits_reactions_for_messages_without_them() {
        // The field is ABSENT (not null, not an empty array) unless the message
        // actually has reactions, so an unreacted read is unchanged.
        let messages = bounded_messages(&reacted_history_response(), 1200);
        let unreacted = messages[1].as_object().expect("message object");
        assert!(
            !unreacted.contains_key("reactions"),
            "an unreacted message carries no reaction summary: {unreacted:?}"
        );
        assert!(
            !unreacted.contains_key("reactions_truncated"),
            "an unreacted message carries no truncation marker: {unreacted:?}"
        );
        // A response where NO message has reactions produces no text suffix at
        // all, so existing read wording is byte-identical.
        let plain = bounded_messages(
            &json!({ "messages": [{ "ts": "1.0", "user": "U1", "text": "hi" }] }),
            1200,
        );
        assert_eq!(reaction_text_suffix(&plain), "");
        // An explicitly empty reactions array is treated as no reactions.
        let empty = bounded_messages(
            &json!({
                "messages": [{ "ts": "1.0", "user": "U1", "text": "hi", "reactions": [] }]
            }),
            1200,
        );
        assert!(
            !empty[0]
                .as_object()
                .expect("object")
                .contains_key("reactions")
        );
    }

    #[test]
    fn read_output_caps_reaction_entries_per_message() {
        // Slack allows dozens of distinct reactions on one message; read output
        // is capped the same way message bodies are, with a truncation marker.
        let many = (0..MAX_MESSAGE_REACTIONS + 4)
            .map(|index| json!({ "name": format!("emoji_{index}"), "count": 1 }))
            .collect::<Vec<_>>();
        let messages = bounded_messages(
            &json!({
                "messages": [{
                    "ts": "1782957738.004499",
                    "user": "U1",
                    "text": "many reactions",
                    "reactions": many
                }]
            }),
            1200,
        );
        assert_eq!(
            messages[0]["reactions"].as_array().map(Vec::len),
            Some(MAX_MESSAGE_REACTIONS),
            "reaction entries are capped at MAX_MESSAGE_REACTIONS"
        );
        assert_eq!(messages[0]["reactions_truncated"], true);
    }

    #[test]
    fn read_thread_and_read_channel_share_the_same_reaction_shape() {
        // read_thread reads conversations.replies and read_channel reads
        // conversations.history; both return {"messages": [...]} and both are
        // shaped by the SAME bounded_messages construction site, so reaction
        // metadata cannot be present in one read tool and missing in the other.
        let replies = reacted_history_response();
        let history = reacted_history_response();
        assert_eq!(
            bounded_messages(&replies, 1200),
            bounded_messages(&history, 1200)
        );
    }

    #[test]
    fn search_requires_user_token_rejects_bot_token() {
        let error = search_requires_user_token("xoxb-abc")
            .expect_err("bot token must be rejected for search.messages");
        assert_eq!(error.message, "slack_search_requires_user_token");
    }

    #[test]
    fn search_requires_user_token_accepts_user_token() {
        assert!(
            search_requires_user_token("xoxp-abc").is_ok(),
            "a valid xoxp- user token must not be rejected"
        );
    }
}
