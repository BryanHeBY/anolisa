#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_EXTENSION_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
EXTENSION_DIR="${1:-${DEFAULT_EXTENSION_DIR}}"
QWEN_BIN="${QWEN_BIN:-qwen}"

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

require_command() {
    local command_name="$1"
    command -v "${command_name}" >/dev/null 2>&1 || fail "${command_name} is not available in PATH"
}

absolute_path() {
    python3 -c \
        'import os, sys; print(os.path.abspath(os.path.expanduser(sys.argv[1])))' \
        "$1"
}

resolve_executable() {
    local command_name="$1"
    local command_path
    command_path="$(command -v "${command_name}" 2>/dev/null)" || \
        fail "${command_name} is not available in PATH"
    [[ -f "${command_path}" && -x "${command_path}" ]] || \
        fail "${command_name} does not resolve to an executable file: ${command_path}"
    absolute_path "${command_path}"
}

json_field() {
    local json_path="$1"
    local field_name="$2"
    python3 -c \
        'import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    value = json.load(stream).get(sys.argv[2], "")
print(value if isinstance(value, str) else "")' \
        "${json_path}" "${field_name}"
}

require_command python3
QWEN_BIN="$(resolve_executable "${QWEN_BIN}")"
AGENT_SEC_CLI_BIN="$(resolve_executable agent-sec-cli)"

if command -v node >/dev/null 2>&1; then
    NODE_VERSION="$(node --version 2>/dev/null || true)"
    if [[ "${NODE_VERSION}" =~ ^v?([0-9]+) ]] && ((BASH_REMATCH[1] < 22)); then
        fail "Qwen Code 0.19.9 requires Node.js >=22; found ${NODE_VERSION}"
    fi
fi
if ! QWEN_VERSION="$("${QWEN_BIN}" --version 2>/dev/null)"; then
    fail "qwen failed to start; verify the Qwen Code installation and its Node.js runtime"
fi
SEMVER_PATTERN='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
[[ "${QWEN_VERSION}" =~ ${SEMVER_PATTERN} ]] || \
    fail "unexpected qwen --version output; expected a Qwen Code semantic version"

if ! QWEN_EXTENSIONS_HELP="$("${QWEN_BIN}" extensions --help 2>&1)"; then
    fail "qwen does not provide the extension management interface"
fi
for REQUIRED_FRAGMENT in \
    "Manage Qwen Code extensions." \
    "qwen extensions install <source>" \
    "qwen extensions update" \
    "qwen extensions enable"; do
    [[ "${QWEN_EXTENSIONS_HELP}" == *"${REQUIRED_FRAGMENT}"* ]] || \
        fail "qwen extensions --help does not match the required Qwen Code interface"
done

if ! AGENT_SEC_CLI_VERSION="$("${AGENT_SEC_CLI_BIN}" --version 2>/dev/null)"; then
    fail "agent-sec-cli failed to start"
fi
AGENT_SEC_CLI_VERSION_PATTERN='^agent-sec-cli[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
[[ "${AGENT_SEC_CLI_VERSION}" =~ ${AGENT_SEC_CLI_VERSION_PATTERN} ]] || \
    fail "unexpected agent-sec-cli --version output"

if ! "${AGENT_SEC_CLI_BIN}" observability schema 2>/dev/null | python3 -c \
    'import json, sys
schema = json.load(sys.stdin)
mapping = schema.get("discriminator", {}).get("mapping", {})
required = {"before_agent_run", "before_tool_call", "after_tool_call", "after_agent_run"}
raise SystemExit(0 if isinstance(mapping, dict) and required <= mapping.keys() else 1)'; then
    fail "agent-sec-cli observability schema is incompatible with this extension"
fi
if ! "${AGENT_SEC_CLI_BIN}" observability record --help >/dev/null 2>&1; then
    fail "agent-sec-cli does not provide observability record"
fi
if ! "${AGENT_SEC_CLI_BIN}" scan-pii --help >/dev/null 2>&1; then
    fail "agent-sec-cli does not provide scan-pii"
fi

echo "Using Qwen Code ${QWEN_VERSION} from ${QWEN_BIN}"
echo "Using ${AGENT_SEC_CLI_VERSION} from ${AGENT_SEC_CLI_BIN}"

EXTENSION_DIR="$(absolute_path "${EXTENSION_DIR}")"
MANIFEST_PATH="${EXTENSION_DIR}/qwen-extension.json"
HOOK_PATH="${EXTENSION_DIR}/hooks/observability_hook.py"

[[ -f "${MANIFEST_PATH}" ]] || fail "missing extension manifest: ${MANIFEST_PATH}"
[[ -f "${HOOK_PATH}" ]] || fail "missing observability hook: ${HOOK_PATH}"

EXTENSION_NAME="$(json_field "${MANIFEST_PATH}" name)"
EXTENSION_VERSION="$(json_field "${MANIFEST_PATH}" version)"
[[ -n "${EXTENSION_NAME}" ]] || fail "qwen-extension.json must define a string name"
[[ -n "${EXTENSION_VERSION}" ]] || fail "qwen-extension.json must define a string version"

QWEN_HOME_DIR="$(absolute_path "${QWEN_HOME:-${HOME}/.qwen}")"
TARGET_DIR="${QWEN_HOME_DIR}/extensions/${EXTENSION_NAME}"
TARGET_MANIFEST="${TARGET_DIR}/qwen-extension.json"
INSTALL_METADATA="${TARGET_DIR}/.qwen-extension-install.json"

if [[ -d "${TARGET_DIR}" ]]; then
    [[ -f "${TARGET_MANIFEST}" ]] || fail \
        "existing extension is not a copied local install; uninstall it before deploying"
    [[ -f "${INSTALL_METADATA}" ]] || fail \
        "existing extension has no install metadata; uninstall it before deploying"

    INSTALL_TYPE="$(json_field "${INSTALL_METADATA}" type)"
    INSTALL_SOURCE="$(json_field "${INSTALL_METADATA}" source)"
    [[ "${INSTALL_TYPE}" == "local" ]] || fail \
        "existing extension uses install type '${INSTALL_TYPE:-unknown}'; uninstall it before deploying"
    [[ -n "${INSTALL_SOURCE}" ]] || fail "existing extension install metadata has no source"

    INSTALL_SOURCE="$(absolute_path "${INSTALL_SOURCE}")"
    [[ "${INSTALL_SOURCE}" == "${EXTENSION_DIR}" ]] || fail \
        "existing extension points to ${INSTALL_SOURCE}; uninstall it before deploying from ${EXTENSION_DIR}"

    INSTALLED_VERSION="$(json_field "${TARGET_MANIFEST}" version)"
    if [[ "${INSTALLED_VERSION}" == "${EXTENSION_VERSION}" ]]; then
        echo "Qwen Code extension ${EXTENSION_NAME} ${EXTENSION_VERSION} is already installed."
    else
        echo "Updating ${EXTENSION_NAME}: ${INSTALLED_VERSION:-unknown} -> ${EXTENSION_VERSION}"
        "${QWEN_BIN}" extensions update "${EXTENSION_NAME}"
    fi
else
    echo "Installing ${EXTENSION_NAME} ${EXTENSION_VERSION} from ${EXTENSION_DIR}"
    "${QWEN_BIN}" extensions install "${EXTENSION_DIR}" --consent --scope user
fi

[[ -f "${TARGET_MANIFEST}" ]] || fail "Qwen Code did not create ${TARGET_MANIFEST}"
DEPLOYED_NAME="$(json_field "${TARGET_MANIFEST}" name)"
DEPLOYED_VERSION="$(json_field "${TARGET_MANIFEST}" version)"
[[ "${DEPLOYED_NAME}" == "${EXTENSION_NAME}" ]] || fail \
    "deployed extension name is '${DEPLOYED_NAME}', expected '${EXTENSION_NAME}'"
[[ "${DEPLOYED_VERSION}" == "${EXTENSION_VERSION}" ]] || fail \
    "deployed extension version is '${DEPLOYED_VERSION}', expected '${EXTENSION_VERSION}'"

"${QWEN_BIN}" extensions enable --scope user "${EXTENSION_NAME}"

echo "Deployed ${EXTENSION_NAME} ${EXTENSION_VERSION} to ${TARGET_DIR}"
echo "Restart running Qwen Code sessions to load the extension."
