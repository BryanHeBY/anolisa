#!/usr/bin/env bash
# Deploy agent-sec-core Hermes plugin to ~/.hermes/plugins/
# Usage: ./scripts/deploy.sh [PLUGIN_DIR]
# Supports: fresh install / upgrade (overwrite) / RPM post-install invocation

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="${1:-$(dirname "$SCRIPT_DIR")}"

# Convert to absolute path if relative
PLUGIN_DIR="$(cd "$PLUGIN_DIR" && pwd)"
SRC_DIR="$PLUGIN_DIR/src"

TARGET_DIR="$HOME/.hermes/plugins/agent-sec-core-hermes-plugin"

# 1. Pre-checks
command -v hermes >/dev/null 2>&1 || { echo "ERROR: hermes not found on PATH"; exit 1; }
command -v agent-sec-cli >/dev/null 2>&1 || { echo "ERROR: agent-sec-cli not found on PATH"; exit 1; }
[[ -f "$SRC_DIR/plugin.yaml" ]] || { echo "ERROR: plugin.yaml not found: $SRC_DIR"; exit 1; }

PLUGIN_VERSION=$(grep '^version:' "$SRC_DIR/plugin.yaml" | awk '{print $2}')
echo "Deploying plugin: agent-sec-core-hermes-plugin v${PLUGIN_VERSION}"
echo "  Source: $SRC_DIR"

# 2. Copy src/ contents to Hermes plugin directory
mkdir -p "$TARGET_DIR"
cp -rp "$SRC_DIR"/. "$TARGET_DIR/"

echo "  ✓ Plugin installed to $TARGET_DIR"

# 3. Enable plugin
hermes plugins enable agent-sec-core-hermes-plugin
echo ""
echo "Note: Please restart Hermes to load the plugin"
