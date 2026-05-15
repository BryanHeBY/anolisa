"""Code-scan capability — scans terminal/execute_code via agent-sec-cli."""

from __future__ import annotations

import json
import logging

from ..cli_runner import call_agent_sec_cli
from ..registry import safe_hook_wrapper

logger = logging.getLogger("agent-sec-core")

_DEFAULT_TIMEOUT = 10.0

# Mapping: tool_name -> (args_key, language)
_TOOL_LANGUAGE_MAP = {
    "terminal": ("command", "bash"),
    "execute_code": ("code", "python"),
}


class CodeScanCapability:
    """Security capability that scans code before execution.

    Intercepts pre_tool_call for 'terminal' and 'execute_code' tools,
    sends the code to agent-sec-cli scan-code, and blocks on deny/warn verdicts.
    """

    id = "code-scan"
    name = "Code Scanner"
    hooks = ["pre_tool_call"]

    def __init__(self):
        self._timeout = _DEFAULT_TIMEOUT
        self._enable_block = False

    def register(self, ctx, config: dict) -> None:
        """Register pre_tool_call hook with safe wrapper."""
        self._timeout = config.get("timeout", _DEFAULT_TIMEOUT)
        self._enable_block = config.get("enable_block", False)
        wrapped = safe_hook_wrapper(self._on_pre_tool_call, self.id)
        ctx.register_hook("pre_tool_call", wrapped)

    def _on_pre_tool_call(self, tool_name, args, **kwargs):
        """Hook handler: scan terminal/execute_code for security risks."""
        # 1. Only intercept known tools
        tool_info = _TOOL_LANGUAGE_MAP.get(tool_name)
        if tool_info is None:
            return None

        args_key, language = tool_info

        # 2. Extract code content
        code = (args or {}).get(args_key, "").strip()
        if not code:
            return None

        # 3. Call agent-sec-cli scan-code
        result = call_agent_sec_cli(
            ["scan-code", "--code", code, "--language", language],
            timeout=self._timeout,
        )

        # 4. Parse result (fail-open on errors)
        if result.exit_code != 0:
            return None

        try:
            scan = json.loads(result.stdout)
        except (json.JSONDecodeError, ValueError):
            return None

        verdict = scan.get("verdict", "pass")

        # warn and deny are separate branches (coding convention), same behavior
        if verdict == "deny":
            logger.warning(f"DENY tool={tool_name} code={code[:120]}")
            if self._enable_block:
                return {"action": "block", "message": self._format_message(scan)}
            return None

        if verdict == "warn":
            logger.warning(f"WARN tool={tool_name} code={code[:120]}")
            if self._enable_block:
                return {"action": "block", "message": self._format_message(scan)}
            return None

        logger.info(f"PASS tool={tool_name} code={code[:120]}")
        return None

    def _format_message(self, scan: dict) -> str:
        """Format scan-code result into a human-readable block message."""
        summary = scan.get("summary", "")
        findings = scan.get("findings", [])
        lines = [
            f"[agent-sec] {summary}" if summary else "[agent-sec] Code scan blocked"
        ]
        for f in findings:
            rule_id = f.get("rule_id", "?")
            desc = f.get("desc_zh") or f.get("desc_en", "")
            lines.append(f"  - {rule_id}: {desc}")
        return "\n".join(lines)
