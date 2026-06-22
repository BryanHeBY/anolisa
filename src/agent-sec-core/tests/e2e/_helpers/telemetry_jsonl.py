"""Shared JSONL helpers for e2e telemetry assertions."""

import json
import time
from pathlib import Path


def read_jsonl_payloads(path: Path) -> list[dict]:
    """Read JSONL payloads, ignoring malformed diagnostic lines."""
    if not path.exists():
        return []

    payloads = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            payloads.append(payload)
    return payloads


def wait_for_telemetry_record(
    path: Path,
    *,
    trace_id: str,
    event_type: str,
    timeout_seconds: float = 5,
) -> dict:
    """Return a telemetry record matching the trace id and event type."""
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        for payload in read_jsonl_payloads(path):
            if (
                payload.get("seccore.trace_id") == trace_id
                and payload.get("seccore.event_type") == event_type
            ):
                return payload
        time.sleep(0.1)
    raise AssertionError(
        f"telemetry record not written for trace_id={trace_id!r} "
        f"event_type={event_type!r}; payloads={read_jsonl_payloads(path)!r}"
    )
