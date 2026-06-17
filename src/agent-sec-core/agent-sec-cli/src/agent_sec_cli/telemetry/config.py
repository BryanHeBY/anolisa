"""Telemetry component metadata."""

from agent_sec_cli import __version__

COMPONENT_NAME = "agent-sec-core"
COMPONENT_AGENT_NAME = ""


def get_component_fields() -> dict[str, str]:
    """Return fixed Agentic OS component fields for telemetry records."""
    return {
        "component.name": COMPONENT_NAME,
        "component.version": __version__,
        "component.agent_name": COMPONENT_AGENT_NAME,
    }
