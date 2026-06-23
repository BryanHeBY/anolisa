#!/usr/bin/env python3
"""Codex UserPromptSubmit hook for prompt-scanner.

Reads a Codex UserPromptSubmit JSON from stdin, extracts the user prompt,
invokes ``agent-sec-cli scan-prompt`` via subprocess, and writes a Codex
HookOutput JSON to stdout.

Modes (controlled by PROMPT_SCANNER_MODE env var, default: observe):
  - observe: silent pass-through, only audit trail via agent-sec-cli events.
            Even if prompt injection is detected, it will NOT be blocked.
  - deny: block prompt with reason when risk is detected.
          (agent-sec-cli's "warn" verdict is escalated to block in this mode)

Usage::

    python3 prompt_scanner_hook.py          # reads stdin, writes stdout

This script is intentionally self-contained — it does NOT import any
``agent_sec_cli`` package.  All it needs is the standard library and the
``agent-sec-cli`` binary on $PATH.
"""

import json
import os
import subprocess
import sys

from trace_context import with_trace_context

# -- config ----------------------------------------------------------------

MODE = os.environ.get("PROMPT_SCANNER_MODE", "observe").lower()
TIMEOUT = int(os.environ.get("PROMPT_SCANNER_TIMEOUT", "10"))
_DEFAULT_SCAN_MODE = "standard"
_DEFAULT_SOURCE = "user_input"


# -- output helpers --------------------------------------------------------


def _block(scan_result: dict) -> None:
    """Output block decision to reject prompt (mode=deny)."""
    threat_type = scan_result.get("threat_type", "")
    risk_level = scan_result.get("risk_level", "unknown")
    confidence = scan_result.get("confidence")

    lines = [
        "[prompt-scanner] \U0001f6ab \u5b89\u5168\u62e6\u622a\uff1a\u68c0\u6d4b\u5230\u63d0\u793a\u8bcd\u6ce8\u5165\u653b\u51fb",
        f"  \u653b\u51fb\u7c7b\u578b : {threat_type or 'unknown'}",
        f"  \u98ce\u9669\u7b49\u7ea7 : {risk_level}",
        f"  \u62e6\u622a\u73af\u8282 : \u7528\u6237\u8f93\u5165\u626b\u63cf (UserPromptSubmit)",
    ]
    if confidence is not None:
        try:
            lines.append(f"  \u6a21\u578b\u7f6e\u4fe1\u5ea6: {float(confidence) * 100:.1f}%")
        except (TypeError, ValueError):
            pass
    lines.append("\u8be5\u63d0\u793a\u8bcd\u5df2\u88ab\u5b89\u5168\u7b56\u7565\u963b\u6b62\uff0c\u8bf7\u4fee\u6539\u540e\u91cd\u8bd5\u3002")
    msg = "\n".join(lines)
    print(json.dumps({"decision": "block", "reason": msg}, ensure_ascii=False))


# -- main ------------------------------------------------------------------


def main() -> None:
    # 1. Read stdin JSON (fail-open: empty stdout = allow in Codex)
    try:
        input_data = json.load(sys.stdin)
    except (json.JSONDecodeError, EOFError, ValueError):
        return

    # 2. Extract user prompt text
    prompt_text = input_data.get("prompt", "")
    if not prompt_text or not isinstance(prompt_text, str) or not prompt_text.strip():
        return  # nothing to scan, allow

    # 3. Call agent-sec-cli scan-prompt via subprocess
    try:
        cmd = with_trace_context(
            [
                "agent-sec-cli",
                "scan-prompt",
                "--text",
                prompt_text,
                "--mode",
                _DEFAULT_SCAN_MODE,
                "--format",
                "json",
                "--source",
                _DEFAULT_SOURCE,
            ],
            input_data,
        )
        proc = subprocess.run(
            cmd,
            capture_output=True,
            check=False,
            text=True,
            timeout=TIMEOUT,
        )
    except Exception:
        return  # fail-open on subprocess error

    if proc.returncode != 0:
        return  # fail-open on CLI error

    # 4. Parse ScanResult JSON from stdout
    try:
        scan_result = json.loads(proc.stdout)
    except (json.JSONDecodeError, ValueError):
        return  # fail-open on parse error

    # 5. Mode-based output
    verdict = scan_result.get("verdict", "pass")

    if verdict in ("pass", "error"):
        return  # allow (fail-open for error)

    # verdict is "warn" or "deny":
    # - agent-sec-cli \u8fd4\u56de warn \u8868\u793a\u6709\u98ce\u9669\u4f46\u4e0d\u4e25\u91cd
    # - agent-sec-cli \u8fd4\u56de deny \u8868\u793a\u9ad8\u5371
    # \u5728\u63d2\u4ef6\u5c42\u7edf\u4e00\u5904\u7406\uff1a\u56e0\u4e3a Codex UserPromptSubmit \u4e0d\u652f\u6301 ask/warn \u900f\u51fa\uff0c
    # \u53ea\u80fd block \u6216\u653e\u884c\uff0c\u6240\u4ee5 warn \u5347\u7ea7\u4e3a block\uff08\u4e0e deny \u540c\u7b49\u5bf9\u5f85\uff09\u3002
    if MODE == "observe":
        return  # observe \u6a21\u5f0f\uff1a\u4e0d\u62e6\u622a\uff0c\u4ec5\u901a\u8fc7 agent-sec-cli events \u5ba1\u8ba1
    elif MODE == "deny":
        _block(scan_result)  # warn \u548c deny \u5747\u62e6\u622a\uff0creason \u5c55\u793a\u7ed9\u7528\u6237
    # else: unknown mode, fail-open


if __name__ == "__main__":
    main()
