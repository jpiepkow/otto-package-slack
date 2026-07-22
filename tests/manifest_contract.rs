use otto_extension_sdk::roles::ToolRegistration;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use toml::Value;

#[test]
fn slack_package_manifest_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = root.join("otto.toml");
    let manifest = std::fs::read_to_string(&manifest_path).expect("read otto.toml");
    let manifest = toml::from_str::<Value>(&manifest).expect("parse otto.toml");

    assert_eq!(manifest["package_id"].as_str(), Some("com.otto.slack"));
    assert_eq!(
        manifest["protocol_version"].as_str(),
        Some("otto.extension.rpc.v1")
    );
    assert_file_exists(&root, manifest["icon"].as_str().expect("icon path"));

    let runtime = manifest["runtime"].as_table().expect("runtime section");
    assert_eq!(runtime["command"].as_str(), Some("bin/otto-tool-slack"));
    assert_eq!(runtime["args"].as_array().map(Vec::len), Some(0));

    let build = manifest["build"].as_table().expect("build section");
    assert_eq!(
        build["command"].as_str(),
        Some(
            "cargo build --release && mkdir -p bin && cp target/release/otto-tool-slack bin/otto-tool-slack"
        )
    );

    let provides = manifest["provides"].as_table().expect("provides section");
    for section in ["tools", "triggers", "channel", "notification"] {
        assert_eq!(
            provides[section]["version"].as_integer(),
            Some(1),
            "missing provides.{section}"
        );
    }

    let schema_ids = collect_ids(&manifest, "schemas");
    for schema_id in [
        "schema.default.slack.setup_details",
        "schema.default.slack.grant_scope",
        "schema.default.slack.message_event",
        "schema.default.slack.read_thread_input",
        "schema.default.slack.read_thread_output",
        "schema.default.slack.search_messages_input",
        "schema.default.slack.add_reaction_input",
        "schema.default.slack.send_message_input",
        "schema.default.slack.send_message_output",
        "schema.default.slack.redaction_input",
        "schema.default.slack.setup_form",
        "schema.default.slack.grant_form",
    ] {
        assert!(
            schema_ids.contains(&schema_id),
            "missing schema {schema_id}"
        );
    }
    for schema in manifest["schemas"].as_array().expect("schemas array") {
        assert_file_exists(&root, schema["path"].as_str().expect("schema path"));
    }

    let tool_ids = collect_ids(&manifest, "tools");
    for tool_id in [
        "tool.default.slack.read_thread",
        "tool.default.slack.read_channel",
        "tool.default.slack.read_recent_dms",
        "tool.default.slack.list_conversations",
        "tool.default.slack.list_users",
        "tool.default.slack.open_dm",
        "tool.default.slack.search_messages",
        "tool.default.slack.add_reaction",
        "tool.default.slack.send_message",
    ] {
        assert!(tool_ids.contains(&tool_id), "missing tool {tool_id}");
    }

    let triggers = manifest["triggers"].as_array().expect("triggers array");
    assert_eq!(triggers.len(), 1);
    assert_eq!(
        triggers[0]["id"].as_str(),
        Some("trigger.default.slack.message")
    );
    assert_eq!(
        triggers[0]["required_scope_fields"]
            .as_array()
            .and_then(|fields| fields.first())
            .and_then(Value::as_str),
        Some("trigger_channel_ids")
    );

    let ui_forms = collect_ids(&manifest, "ui_forms");
    assert!(ui_forms.contains(&"slack_setup"));
    assert!(ui_forms.contains(&"slack_grant"));
    assert_file_exists(&root, "ui/setup.form.json");
    assert_file_exists(&root, "ui/grant.form.json");

    let redaction = collect_ids(&manifest, "redaction");
    assert!(redaction.contains(&"redaction.default.slack.message_content"));
}

#[derive(Debug, Deserialize)]
struct ManifestTools {
    tools: Vec<ToolRegistration>,
}

/// Reads the manifest's `[[tools]]` through the very type Otto deserializes it
/// with, so serde defaults land exactly as they do in the control plane.
fn manifest_tool_registrations() -> Vec<ToolRegistration> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("otto.toml")).expect("read otto.toml");
    toml::from_str::<ManifestTools>(&manifest)
        .expect("otto.toml [[tools]] must deserialize as ToolRegistration")
        .tools
}

/// The manifest is the tool metadata Otto actually stores and enforces on.
///
/// `otto-extension-runtime`'s `persist_scan_report` seeds the registration rows
/// from `ExtensionRegistrations::from_manifest(&manifest)`, and the only code
/// path that would replace them with the runtime's `registrations.get` response
/// (`refresh_registrations`) has no production caller. Core's
/// `validate_registrations_within_manifest` only checks that runtime IDs stay
/// within the manifest's ceiling - it never compares annotation *values* - so
/// the two copies can disagree indefinitely and nothing upstream notices.
///
/// They did disagree: `src/lib.rs` declared truthful `read_only`/`open_world`
/// hints while `otto.toml` omitted all four fields, leaving every tool at the
/// serde default `false` in the copy the bridge reads. Pin them together.
#[test]
fn slack_manifest_tools_match_the_runtime_registrations_exactly() {
    let manifest_tools = manifest_tool_registrations();
    let runtime_tools = otto_tool_slack::registrations().tools;

    assert_eq!(
        manifest_tools.len(),
        runtime_tools.len(),
        "manifest and runtime must register the same tools"
    );
    for runtime_tool in &runtime_tools {
        let manifest_tool = manifest_tools
            .iter()
            .find(|tool| tool.id == runtime_tool.id)
            .unwrap_or_else(|| panic!("otto.toml is missing tool {}", runtime_tool.id));
        assert_eq!(
            manifest_tool, runtime_tool,
            "otto.toml and registrations() disagree about {}",
            runtime_tool.id
        );
    }
}

/// Bridge audit events carry the manifest copy of these annotations, so assert
/// the Phase 65 matrix there directly rather than only against the runtime copy
/// the bridge never sees.
#[test]
fn slack_manifest_annotations_describe_what_each_tool_actually_does() {
    let tools = manifest_tool_registrations();
    let annotations = |suffix: &str| {
        let id = format!("tool.default.slack.{suffix}");
        let tool = tools
            .iter()
            .find(|tool| tool.id.as_str() == id)
            .unwrap_or_else(|| panic!("otto.toml is missing {id}"));
        (
            tool.read_only,
            tool.destructive,
            tool.idempotent,
            tool.open_world,
        )
    };

    // Reads: no external mutation, and every one of them reaches Slack.
    for tool in [
        "read_thread",
        "read_channel",
        "read_recent_dms",
        "list_conversations",
        "list_users",
        "open_dm",
        "search_messages",
    ] {
        assert_eq!(annotations(tool), (true, false, false, true), "{tool}");
    }

    // Writes: both mutate workspace state visible to other people, and both
    // reach the Slack Web API.
    for tool in ["add_reaction", "send_message"] {
        assert_eq!(annotations(tool), (false, true, false, true), "{tool}");
    }

    // Core derives the approval default from `destructive`, so the two must not
    // drift apart: every mutating tool stays approval-gated.
    for tool in &tools {
        assert_eq!(
            tool.approval_default(),
            tool.destructive,
            "{} approval default must track its destructive hint",
            tool.id
        );
        assert!(
            !(tool.read_only && tool.destructive),
            "{} cannot be both read-only and destructive",
            tool.id
        );
    }
}

fn collect_ids<'a>(manifest: &'a Value, section: &str) -> Vec<&'a str> {
    manifest[section]
        .as_array()
        .unwrap_or_else(|| panic!("{section} array"))
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or("missing-id"))
        .collect()
}

fn assert_file_exists(root: &Path, relative: &str) {
    let path = root.join(relative);
    assert!(path.is_file(), "{} should exist", path.display());
}
