"""Telemetry path and component metadata."""

import os
from pathlib import Path

from agent_sec_cli import __version__
from agent_sec_cli.correlation_context import get_current_trace_context

COMPONENT_NAME = "agent-sec-core"
COMPONENT_AGENT_NAME = ""
DEFAULT_TELEMETRY_LOG_PATH = "/var/log/anolisa/sls/ops/agent-sec-core.jsonl"
TELEMETRY_LOG_PATH_ENV = "AGENT_SEC_TELEMETRY_LOG_PATH"


def get_telemetry_log_path() -> Path:
    """Return the configured Agentic OS telemetry JSONL path."""
    override = os.environ.get(TELEMETRY_LOG_PATH_ENV)
    if override:
        return Path(override).expanduser()
    return Path(DEFAULT_TELEMETRY_LOG_PATH)


def telemetry_log_path_exists() -> bool:
    """Return whether the configured telemetry JSONL file exists."""
    return get_telemetry_log_path().is_file()


def get_component_fields() -> dict[str, str]:
    """Return fixed Agentic OS component fields for telemetry records.

    ``component.agent_name`` is read from the ambient trace context
    (process-level or request-local ContextVar).  Falls back to the
    build-time default when no trace context is active.
    """
    trace_ctx = get_current_trace_context()
    raw_agent_name = trace_ctx.agent_name if trace_ctx else None
    component_agent_name = (
        raw_agent_name.strip() if raw_agent_name else COMPONENT_AGENT_NAME
    )
    return {
        "component.name": COMPONENT_NAME,
        "component.version": __version__,
        "component.agent_name": component_agent_name,
    }
