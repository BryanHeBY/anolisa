"""Unit tests for the Qoder plugin framework shell."""

import importlib.util
import json
import sys
from pathlib import Path

_PLUGIN_DIR = Path(__file__).resolve().parents[3] / "qoder-plugin"
_HOOKS_DIR = _PLUGIN_DIR / "hooks"

_spec = importlib.util.spec_from_file_location(
    "qoder_hook_common", _HOOKS_DIR / "qoder_hook_common.py"
)
qoder_hook_common = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = qoder_hook_common
_spec.loader.exec_module(qoder_hook_common)


def test_plugin_manifest_declares_stable_name() -> None:
    manifest = json.loads((_PLUGIN_DIR / ".qoder-plugin" / "plugin.json").read_text())

    assert manifest["name"] == "agent-sec-core"
    assert manifest["version"] == "0.8.0"


def test_hooks_json_uses_qoder_plugin_wrapper() -> None:
    hooks = json.loads((_HOOKS_DIR / "hooks.json").read_text())

    assert hooks == {"hooks": {}}


def test_common_outputs_qoder_hook_shapes() -> None:
    deny = json.loads(qoder_hook_common.deny_output("blocked"))
    pre_tool = json.loads(qoder_hook_common.pre_tool_decision_output("deny", "nope"))
    post_tool = json.loads(qoder_hook_common.post_tool_output_replacement("redacted"))

    assert deny == {"decision": "deny", "reason": "blocked"}
    assert pre_tool == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "nope",
        }
    }
    assert post_tool == {
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": "redacted",
        }
    }


def test_trace_context_marks_qoder_agent() -> None:
    args = qoder_hook_common.with_trace_context(
        ["agent-sec-cli", "scan-pii"],
        {"session_id": "sess-1", "tool_use_id": "tool-1"},
    )

    assert args[:2] == ["agent-sec-cli", "--trace-context"]
    context = json.loads(args[2])
    assert context == {
        "agent_name": "qoder",
        "session_id": "sess-1",
        "tool_call_id": "tool-1",
    }
