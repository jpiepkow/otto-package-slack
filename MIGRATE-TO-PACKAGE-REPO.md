# Move With Slack Package

Phase 59 keeps the generic core contract coverage on `fixture-omni`. The remaining Slack-specific references in core tests are package behavior and manifest-corpus checks that should move with this package when it leaves the workspace.

## Tests To Relocate

- `crates/otto-extension-runtime/tests/capability_versioning.rs`
  - Covers the first-party manifest corpus entry for `default-tool-slack`, including accepted `provides` metadata and package icon presence.
- `crates/otto-extension-runtime/tests/default_tool_packages.rs`
  - Covers `slack_package_manifest_contract`: scan acceptance for `com.otto.slack`, runtime command `otto-tool-slack`, Slack schema IDs, tool IDs, trigger ID, setup/form IDs, redaction, capabilities, and per-tool approval flags.
- `crates/otto-control-plane/tests/control_api.rs`
  - Covers `extension_catalog_loads_status_and_ui_form_documents`, specifically bundled Slack catalog inspection, `slack_setup`, `slack_grant`, and the Slack grant-scope schema document.

## Core Replacement

Core package-neutral coverage now lives in the Phase 59 fixture tests:

- `crates/otto-extension-runtime/tests/fixtures/extensions/fixture-omni/`
- `crates/otto-control-plane/tests/bridge_*`
- `crates/otto-control-plane/tests/control_api.rs::fixture_trigger_save_is_fail_closed_on_empty_scope_target`
