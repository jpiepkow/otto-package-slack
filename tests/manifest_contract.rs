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
