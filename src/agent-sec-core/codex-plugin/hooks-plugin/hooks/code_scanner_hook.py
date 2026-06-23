#!/usr/bin/env python3
"""Codex PreToolUse hook for code-scanner.

Reads a Codex PreToolUse JSON from stdin, extracts the shell command,
invokes ``agent-sec-cli scan-code`` via subprocess, and writes a Codex
HookOutput JSON to stdout.

Modes (controlled by CODE_SCANNER_MODE env var, default: observe):
  - observe: silent pass-through, only audit trail via agent-sec-cli events.
            Even if dangerous commands are detected, they will NOT be blocked.
  - deny: block execution with reason when risk is detected.
          (agent-sec-cli's "warn" verdict is escalated to block in this mode)

Self-protect: regardless of mode, commands matching shell-self-protect-*
rules are always blocked.

Usage::

    python3 code_scanner_hook.py          # reads stdin, writes stdout

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

MODE = os.environ.get("CODE_SCANNER_MODE", "observe").lower()
TIMEOUT = int(os.environ.get("CODE_SCANNER_TIMEOUT", "10"))
_DEFAULT_LANGUAGE = "bash"


# -- output helpers --------------------------------------------------------


def _block(findings: list[dict]) -> None:
    """Output block decision to prevent execution (mode=deny)."""
    descs = [
        f"- {f.get('rule_id', 'unknown')}: {f.get('desc_zh', f.get('desc_en', ''))}"
        for f in findings
    ]
    msg = (
        f"[code-scanner] \U0001f6ab 安全拦截\uff1a检测到 {len(findings)} 个风险项:\n"
        + "\n".join(descs)
        + "\n\n该命令已被安全策略阻止。"
    )
    print(json.dumps({"decision": "block", "reason": msg}, ensure_ascii=False))


def _block_self_protect(command: str) -> None:
    """Force block for self-protect rule hits, regardless of mode."""
    msg = (
        "[code-scanner] \U0001f6e1\ufe0f 自我保护\uff1a该命令将禁用 agent-sec 安全插件。\n"
        "如果您确实需要禁用\uff0c请手动执行以下命令\uff1a\n\n"
        f"  {command}\n\n"
        "出于安全原因\uff0cAI agent 无法执行此操作。"
    )
    print(json.dumps({"decision": "block", "reason": msg}, ensure_ascii=False))


# -- main ------------------------------------------------------------------


def main() -> None:
    # 1. Read stdin JSON (fail-open: empty stdout = allow in Codex)
    try:
        input_data = json.load(sys.stdin)
    except (json.JSONDecodeError, EOFError, ValueError):
        return

    # 2. Extract command from tool_input
    #    Codex normalizes all shell tools to tool_name="Bash",
    #    tool_input={"command": "..."}
    tool_input = input_data.get("tool_input", {})
    command = tool_input.get("command", "")

    if not command or not isinstance(command, str) or not command.strip():
        return  # nothing to scan, allow

    # 3. Call agent-sec-cli via subprocess
    try:
        cmd = with_trace_context(
            [
                "agent-sec-cli",
                "scan-code",
                "--code",
                command,
                "--language",
                _DEFAULT_LANGUAGE,
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

    # 5. Self-protect check (highest priority, ignores MODE)
    findings = scan_result.get("findings", [])
    self_protect = next(
        (f for f in findings if f.get("rule_id", "").startswith("shell-self-protect")),
        None,
    )
    if self_protect is not None:
        _block_self_protect(command)
        return

    # 6. Mode-based output
    verdict = scan_result.get("verdict", "pass")

    if verdict in ("pass", "error"):
        return  # allow (fail-open for error)

    # verdict is "warn" or "deny":
    # - agent-sec-cli 返回 warn 表示有风险但不严重
    # - agent-sec-cli 返回 deny 表示高危
    # 在插件层统一处理：因为 Codex PreToolUse 不支持 ask/warn 透出，
    # 只能 block 或放行，所以 warn 升级为 block（与 deny 同等对待）。
    if MODE == "observe":
        return  # observe 模式：不拦截，仅通过 agent-sec-cli events 审计
    elif MODE == "deny":
        _block(findings)  # warn 和 deny 均拦截
    # else: unknown mode, fail-open


if __name__ == "__main__":
    main()
