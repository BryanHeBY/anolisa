#!/usr/bin/env python3
"""Codex UserPromptSubmit hook for skill-ledger.

Reads a Codex UserPromptSubmit JSON from stdin, parses ``$skill-name``
mentions from the user prompt, resolves each to an installed skill directory,
invokes ``agent-sec-cli skill-ledger check`` for each, and writes a Codex
HookOutput JSON to stdout.

Hook point: **UserPromptSubmit** (no matcher — fires on every prompt)

Modes (controlled by SKILL_LEDGER_MODE env var, default: observe):
  - observe: silent pass-through, only audit trail via agent-sec-cli events.
            Even if integrity check fails, it will NOT be blocked.
  - deny: block the entire turn when any skill fails integrity check.
          (status none/drifted/warn/deny/tampered all result in block)

Input schema::

    {
        "session_id": "thread_xxx",
        "turn_id": "turn_xxx",
        "cwd": "/current/working/directory",
        "hook_event_name": "UserPromptSubmit",
        "model": "...",
        "permission_mode": "default",
        "prompt": "$my-skill 帮我重构代码"
    }

Output mapping (mode=deny):

    all skills pass          → (empty stdout — allow)
    any skill status in BLOCK_STATUSES → {"decision": "block", "reason": "..."}

Output mapping (mode=observe):

    always                   → (empty stdout — allow, audit only)

Usage::

    python3 skill_ledger_hook.py          # reads stdin, writes stdout

This script is intentionally self-contained — it does NOT import any
``agent_sec_cli`` package.  All it needs is the standard library and the
``agent-sec-cli`` binary on $PATH.
"""

import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from trace_context import with_trace_context

# -- config ----------------------------------------------------------------

MODE = os.environ.get("SKILL_LEDGER_MODE", "observe").lower()
TIMEOUT = int(os.environ.get("SKILL_LEDGER_TIMEOUT", "5"))
_INIT_TIMEOUT = 3  # seconds for key initialization

# -- constants -------------------------------------------------------------

# Statuses that should trigger block in deny mode.
# Mirrors agent-sec-cli skill-ledger check output status values.
_BLOCK_STATUSES = frozenset({"none", "drifted", "warn", "deny", "tampered"})

# Regex to extract $skill-name mentions from prompt.
# Matches: $ followed by letter, then [a-zA-Z0-9_:-]*
# Consistent with Codex's `extract_tool_mentions_with_sigil` in injection.rs.
_MENTION_RE = re.compile(r"\$([a-zA-Z][a-zA-Z0-9_:\-]*)")

# Common environment variable names to exclude from skill matching.
# Prevents $PATH, $HOME etc. from being treated as skill mentions.
_COMMON_ENV_VARS = frozenset(
    {
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "PWD",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "TERM",
        "EDITOR",
        "VISUAL",
        "PAGER",
        "DISPLAY",
        "HOSTNAME",
        "LOGNAME",
        "MAIL",
        "OLDPWD",
        "SHLVL",
        "XDG_DATA_HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "CODEX_HOME",
    }
)

_STATUS_LABELS = {
    "none": "从未扫描",
    "drifted": "文件内容已变更",
    "warn": "扫描有低风险发现",
    "deny": "扫描有高风险发现",
    "tampered": "签名验证失败",
}


# -- skill directory resolution --------------------------------------------


def _skill_roots(cwd: str) -> list[Path]:
    """Return known Codex skill root directories.

    Mirrors Codex's `skill_roots_from_layer_stack_inner` in loader.rs:
    - Repository level: <project>/.agents/skills/
    - User level: $CODEX_HOME/skills/ and $HOME/.agents/skills/
    - System level: /etc/codex/skills/
    """
    codex_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    return [
        Path(cwd) / ".agents" / "skills",
        codex_home / "skills",
        Path.home() / ".agents" / "skills",
        Path("/etc/codex/skills"),
    ]


def _resolve_skill_dir(skill_name: str, cwd: str) -> str | None:
    """Resolve a skill name to its on-disk directory path.

    Returns the resolved path string if found, None otherwise.
    Only returns a path if the directory exists and contains a SKILL.md file.
    """
    for root in _skill_roots(cwd):
        candidate = root / skill_name
        try:
            resolved = candidate.resolve()
        except (OSError, ValueError):
            continue
        if resolved.is_dir() and (resolved / "SKILL.md").is_file():
            return str(resolved)
    return None


# -- prompt parsing --------------------------------------------------------


def _extract_skill_mentions(prompt: str) -> list[str]:
    """Extract $skill-name mentions from prompt, excluding env vars.

    Returns deduplicated list of potential skill names (without $ prefix).
    """
    seen: set[str] = set()
    result: list[str] = []
    for match in _MENTION_RE.findall(prompt):
        if match.upper() in _COMMON_ENV_VARS:
            continue
        if match not in seen:
            seen.add(match)
            result.append(match)
    return result


# -- key management --------------------------------------------------------


def _keys_exist() -> bool:
    """Return True if both key.pub and key.enc exist."""
    xdg_data = os.environ.get("XDG_DATA_HOME", "")
    if not xdg_data:
        xdg_data = str(Path.home() / ".local" / "share")
    data_dir = Path(xdg_data) / "agent-sec" / "skill-ledger"
    return (data_dir / "key.pub").is_file() and (data_dir / "key.enc").is_file()


def _ensure_keys(input_data: dict[str, Any]) -> None:
    """Auto-initialize signing keys if missing (fire-and-forget)."""
    if _keys_exist():
        return
    try:
        cmd = with_trace_context(
            ["agent-sec-cli", "skill-ledger", "init", "--no-baseline"],
            input_data,
        )
        subprocess.run(
            cmd,
            capture_output=True,
            check=False,
            text=True,
            timeout=_INIT_TIMEOUT,
        )
    except Exception:
        pass


# -- output helpers --------------------------------------------------------


def _block(failed_skills: list[tuple[str, str]]) -> None:
    """Output block decision to reject the turn (mode=deny).

    Args:
        failed_skills: list of (skill_name, status) tuples that failed check.
    """
    lines = ["[skill-ledger] \U0001f6ab 技能完整性校验失败："]
    for name, status in failed_skills:
        label = _STATUS_LABELS.get(status, f"状态异常({status})")
        lines.append(f"  - {name}: {label}")
    lines.append("")
    lines.append("请运行以下命令重新认证后重试：")
    for name, _ in failed_skills:
        lines.append(f"  agent-sec-cli skill-ledger scan <{name}_dir>")
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
    prompt = input_data.get("prompt", "")
    if not prompt or not isinstance(prompt, str) or not prompt.strip():
        return  # nothing to check, allow

    # 3. Parse $skill-name mentions from prompt
    mentions = _extract_skill_mentions(prompt)
    if not mentions:
        return  # no skill mentions, allow

    # 4. Resolve mentions to installed skill directories
    cwd = input_data.get("cwd", ".")
    skills_to_check: list[tuple[str, str]] = []  # (name, dir_path)
    for skill_name in mentions:
        skill_dir = _resolve_skill_dir(skill_name, cwd)
        if skill_dir is not None:
            skills_to_check.append((skill_name, skill_dir))

    if not skills_to_check:
        return  # no matching installed skills, allow

    # 5. Ensure signing keys exist (auto-init if missing)
    _ensure_keys(input_data)

    # 6. Check each skill via agent-sec-cli
    failed: list[tuple[str, str]] = []  # (name, status)
    for skill_name, skill_dir in skills_to_check:
        try:
            cmd = with_trace_context(
                ["agent-sec-cli", "skill-ledger", "check", skill_dir],
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
            continue  # fail-open on subprocess error

        try:
            check_result = json.loads(proc.stdout)
        except (json.JSONDecodeError, ValueError):
            continue  # fail-open on parse error

        status = check_result.get("status", "unknown")
        if status in _BLOCK_STATUSES:
            failed.append((skill_name, status))

    if not failed:
        return  # all passed or no checks needed, allow

    # 7. Mode-based output
    # - observe: 不拦截，仅通过 agent-sec-cli events 审计
    # - deny: status 异常时拦截整个 turn（skill 内容不会注入模型上下文）
    if MODE == "observe":
        return  # observe 模式：不拦截
    elif MODE == "deny":
        _block(failed)
    # else: unknown mode, fail-open


if __name__ == "__main__":
    main()
