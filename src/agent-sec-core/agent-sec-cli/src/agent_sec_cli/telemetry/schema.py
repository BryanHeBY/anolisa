"""Build telemetry records from SecurityEvent values."""

import uuid
from typing import Any

from agent_sec_cli.security_events.schema import SecurityEvent
from agent_sec_cli.telemetry.config import get_component_fields
from agent_sec_cli.telemetry.sanitizer import (
    details_dict,
    error_type_value,
    error_value,
    now_iso,
    request_value,
    result_dict,
    result_value,
    value_or_none,
)

_BASELINE_ACTION = "harden"


def build_telemetry_security_event(event: SecurityEvent) -> dict[str, Any]:
    """Build a telemetry JSON record from a canonical SecurityEvent."""
    if event.event_type == _BASELINE_ACTION:
        return _build_baseline_record(event)
    return _build_seccore_record(event)


def _build_seccore_record(event: SecurityEvent) -> dict[str, Any]:
    """Build a seccore.* telemetry record."""
    details = details_dict(event.details)
    result = result_dict(details)

    record: dict[str, Any] = get_component_fields()
    record.update(
        {
            "seccore.event_id": _event_id(event),
            "seccore.event_type": value_or_none(event.event_type),
            "seccore.category": value_or_none(event.category),
            "seccore.result": value_or_none(event.result),
            "seccore.timestamp": _timestamp(event),
            "seccore.trace_id": value_or_none(event.trace_id),
            "seccore.session_id": value_or_none(event.session_id),
            "seccore.run_id": value_or_none(event.run_id),
            "seccore.call_id": value_or_none(event.call_id),
            "seccore.tool_call_id": value_or_none(event.tool_call_id),
            "seccore.request": request_value(details),
            "seccore.error": error_value(details, result),
            "seccore.error_type": error_type_value(details, result),
            "seccore.verdict": result_value(result, "verdict"),
            "seccore.summary": result_value(result, "summary"),
            "seccore.elapsed_ms": result_value(result, "elapsed_ms"),
            "seccore.asset_passed_count": result_value(result, "passed"),
            "seccore.asset_failed_count": result_value(result, "failed"),
            "seccore.details": {},
        }
    )
    return record


def _build_baseline_record(event: SecurityEvent) -> dict[str, Any]:
    """Build a baseline.* telemetry record."""
    details = details_dict(event.details)
    result = result_dict(details)

    record: dict[str, Any] = get_component_fields()
    record.update(
        {
            "baseline.event_id": _event_id(event),
            "baseline.result": value_or_none(event.result),
            "baseline.timestamp": _timestamp(event),
            "baseline.request": request_value(details),
            "baseline.error": error_value(details, result),
            "baseline.error_type": error_type_value(details, result),
            "baseline.passed": result_value(result, "passed"),
            "baseline.fixed": result_value(result, "fixed"),
            "baseline.failed": result_value(result, "failed"),
            "baseline.total": result_value(result, "total"),
            "baseline.details": {},
        }
    )
    return record


def _event_id(event: SecurityEvent) -> str:
    """Return the source event ID or generate a UUID when missing."""
    if event.event_id:
        return event.event_id
    return str(uuid.uuid4())


def _timestamp(event: SecurityEvent) -> str:
    """Return the source timestamp or generate one when missing."""
    if event.timestamp:
        return event.timestamp
    return now_iso()
