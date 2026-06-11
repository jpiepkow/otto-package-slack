# Otto Slack Package

Default Slack tools, channel, notification, and trigger package for Otto.

This package is installed by Otto from GitHub and built locally through the package manifest build hook. It exposes the `com.otto.slack` package contract and runs the `otto-tool-slack` JSON-RPC extension binary.

## Build

```sh
cargo build --release
mkdir -p bin
cp target/release/otto-tool-slack bin/otto-tool-slack
```

## Test

```sh
cargo test
```
