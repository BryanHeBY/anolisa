"""Unit tests for telemetry schema mapping."""

import copy
import json
import uuid
from dataclasses import dataclass
from datetime import datetime
from typing import Any

from agent_sec_cli import __version__
from agent_sec_cli.security_events.schema import SecurityEvent
from agent_sec_cli.telemetry.schema import build_telemetry_security_event

COMPONENT_FIELDS = {
    "component.name",
    "component.version",
    "component.agent_name",
}

SECCORE_FIELDS = {
    "seccore.event_id",
    "seccore.event_type",
    "seccore.category",
    "seccore.result",
    "seccore.timestamp",
    "seccore.trace_id",
    "seccore.session_id",
    "seccore.run_id",
    "seccore.call_id",
    "seccore.tool_call_id",
    "seccore.request",
    "seccore.error",
    "seccore.error_type",
    "seccore.verdict",
    "seccore.summary",
    "seccore.elapsed_ms",
    "seccore.asset_passed_count",
    "seccore.asset_failed_count",
    "seccore.details",
}

BASELINE_FIELDS = {
    "baseline.event_id",
    "baseline.result",
    "baseline.timestamp",
    "baseline.request",
    "baseline.error",
    "baseline.error_type",
    "baseline.passed",
    "baseline.fixed",
    "baseline.failed",
    "baseline.total",
    "baseline.details",
}


@dataclass
class _TelemetryCtx:
    agent_name: str | None = None


def _event(**overrides: Any) -> SecurityEvent:
    defaults: dict[str, Any] = {
        "event_id": "event-1",
        "event_type": "code_scan",
        "category": "code_scan",
        "result": "succeeded",
        "timestamp": "2026-06-15T12:00:00+00:00",
        "trace_id": "trace-1",
        "details": {},
    }
    defaults.update(overrides)
    return SecurityEvent(**defaults)


def _assert_component_fields(record: dict[str, Any], agent_name: str = "") -> None:
    assert record["component.name"] == "agent-sec-core"
    assert record["component.version"] == __version__
    assert record["component.agent_name"] == agent_name


def test_builds_seccore_record_with_component_and_prefixed_fields() -> None:
    event = _event(
        event_type="pii_scan",
        category="pii_scan",
        trace_id="trace-123",
        session_id="session-123",
        run_id="run-123",
        call_id="call-123",
        tool_call_id="tool-123",
        details={
            "request": {"source": "manual", "text_sha256": "abc"},
            "result": {
                "verdict": "deny",
                "summary": {"total": 1},
                "elapsed_ms": 28,
            },
        },
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert set(record) == COMPONENT_FIELDS | SECCORE_FIELDS
    _assert_component_fields(record)
    assert "schema.namespace" not in record
    assert "data.group" not in record
    assert record["seccore.event_id"] == "event-1"
    assert record["seccore.event_type"] == "pii_scan"
    assert record["seccore.category"] == "pii_scan"
    assert record["seccore.result"] == "succeeded"
    assert record["seccore.timestamp"] == "2026-06-15T12:00:00+00:00"
    assert record["seccore.trace_id"] == "trace-123"
    assert record["seccore.session_id"] == "session-123"
    assert record["seccore.run_id"] == "run-123"
    assert record["seccore.call_id"] == "call-123"
    assert record["seccore.tool_call_id"] == "tool-123"
    assert record["seccore.request"] == {"source": "manual", "text_sha256": "abc"}
    assert record["seccore.error"] is None
    assert record["seccore.error_type"] is None
    assert record["seccore.verdict"] == "deny"
    assert record["seccore.summary"] == {"total": 1}
    assert record["seccore.elapsed_ms"] == 28
    assert record["seccore.asset_passed_count"] is None
    assert record["seccore.asset_failed_count"] is None
    assert record["seccore.details"] == {}
    json.dumps(record)


def test_builds_asset_verify_counts_as_seccore_fields() -> None:
    event = _event(
        event_type="verify",
        category="asset_verify",
        result="failed",
        details={"request": {"skill": "/path"}, "result": {"passed": 12, "failed": 1}},
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    _assert_component_fields(record)
    assert record["seccore.asset_passed_count"] == 12
    assert record["seccore.asset_failed_count"] == 1
    assert record["seccore.verdict"] is None


def test_builds_seccore_record_with_ctx_agent_name() -> None:
    record = build_telemetry_security_event(_event(), _TelemetryCtx(agent_name="cosh"))

    _assert_component_fields(record, agent_name="cosh")


def test_builds_seccore_record_strips_ctx_agent_name() -> None:
    record = build_telemetry_security_event(
        _event(), _TelemetryCtx(agent_name=" hermes ")
    )

    _assert_component_fields(record, agent_name="hermes")


def test_builds_baseline_record_with_component_and_prefixed_fields() -> None:
    event = _event(
        event_type="harden",
        category="hardening",
        result="failed",
        details={
            "request": {"args": ["--scan", "--config", "agentos_baseline"]},
            "result": {"passed": 12, "fixed": 0, "failed": 1, "total": 13},
        },
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert set(record) == COMPONENT_FIELDS | BASELINE_FIELDS
    _assert_component_fields(record)
    assert not any(key.startswith("seccore.") for key in record)
    assert "schema.namespace" not in record
    assert "data.group" not in record
    assert record["baseline.event_id"] == "event-1"
    assert record["baseline.result"] == "failed"
    assert record["baseline.timestamp"] == "2026-06-15T12:00:00+00:00"
    assert record["baseline.request"] == {
        "args": ["--scan", "--config", "agentos_baseline"]
    }
    assert record["baseline.error"] is None
    assert record["baseline.error_type"] is None
    assert record["baseline.passed"] == 12
    assert record["baseline.fixed"] == 0
    assert record["baseline.failed"] == 1
    assert record["baseline.total"] == 13
    assert record["baseline.details"] == {}
    json.dumps(record)


def test_builds_baseline_record_with_ctx_agent_name() -> None:
    event = _event(event_type="harden", category="hardening")

    record = build_telemetry_security_event(event, _TelemetryCtx(agent_name="cosh"))

    _assert_component_fields(record, agent_name="cosh")


def test_error_fields_map_from_exception_details() -> None:
    event = _event(
        event_type="verify",
        category="asset_verify",
        result="failed",
        details={
            "request": {"skill": "/path"},
            "error": "boom",
            "error_type": "RuntimeError",
        },
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert record["seccore.error"] == "boom"
    assert record["seccore.error_type"] == "RuntimeError"
    assert record["seccore.request"] == {"skill": "/path"}


def test_error_fields_do_not_fallback_to_result_summary() -> None:
    event = _event(
        event_type="pii_scan",
        category="pii_scan",
        result="failed",
        details={
            "result": {
                "verdict": "error",
                "summary": {"error": "bad input", "error_type": "TypeError"},
            }
        },
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert record["seccore.error"] is None
    assert record["seccore.error_type"] is None
    assert record["seccore.summary"] == {
        "error": "bad input",
        "error_type": "TypeError",
    }


def test_error_fields_preserve_explicit_details_null_over_result_values() -> None:
    event = _event(
        event_type="pii_scan",
        category="pii_scan",
        result="failed",
        details={
            "error": None,
            "error_type": None,
            "result": {
                "error": "ignored",
                "error_type": "IgnoredError",
                "summary": {"error": "bad input", "error_type": "TypeError"},
            },
        },
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert record["seccore.error"] is None
    assert record["seccore.error_type"] is None


def test_missing_fields_use_null_except_generated_event_id_timestamp_and_details() -> (
    None
):
    event = _event(
        event_id="",
        timestamp="",
        trace_id="",
        details={},
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    uuid.UUID(record["seccore.event_id"])
    datetime.fromisoformat(record["seccore.timestamp"])
    assert record["seccore.trace_id"] is None
    assert record["seccore.request"] is None
    assert record["seccore.verdict"] is None
    assert record["seccore.details"] == {}


def test_mapping_does_not_mutate_input_and_converts_values_to_json_safe() -> None:
    details = {
        "request": {"items": ("a", "b")},
        "result": {"summary": {"values": {"z", "a"}}},
    }
    original = copy.deepcopy(details)
    event = _event(details=details)

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert details == original
    assert record["seccore.request"] == {"items": ["a", "b"]}
    assert record["seccore.summary"] == {"values": ["a", "z"]}
    json.dumps(record)


def test_mapping_converts_non_finite_floats_to_null_for_strict_json() -> None:
    event = _event(
        details={
            "request": {
                "nan": float("nan"),
                "values": [float("inf"), float("-inf"), 1.25],
            },
            "error": float("nan"),
            "result": {
                "elapsed_ms": float("inf"),
                "summary": {"score": float("nan")},
            },
        }
    )

    record = build_telemetry_security_event(event, _TelemetryCtx())

    assert record["seccore.request"] == {"nan": None, "values": [None, None, 1.25]}
    assert record["seccore.error"] is None
    assert record["seccore.elapsed_ms"] is None
    assert record["seccore.summary"] == {"score": None}
    json.dumps(record, allow_nan=False)
