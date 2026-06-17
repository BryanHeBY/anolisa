"""Map SecurityEvent details into telemetry business fields."""

import copy
import json
from datetime import datetime, timezone
from typing import Any


def now_iso() -> str:
    """Return the current UTC timestamp in ISO-8601 format."""
    return datetime.now(timezone.utc).isoformat()


def to_json_safe(value: Any) -> Any:
    """Return a deep-copied JSON-safe representation of *value*."""
    copied = copy.deepcopy(value)
    return _make_json_safe(copied)


def details_dict(value: Any) -> dict[str, Any]:
    """Return *value* when it is a dict, otherwise an empty dict."""
    if isinstance(value, dict):
        return value
    return {}


def value_or_none(value: Any) -> Any:
    """Return None for missing string fields encoded as empty strings."""
    if value == "":
        return None
    return value


def result_dict(details: dict[str, Any]) -> dict[str, Any]:
    """Return details.result when it is a dict, otherwise an empty dict."""
    result = details.get("result")
    if isinstance(result, dict):
        return result
    return {}


def request_value(details: dict[str, Any]) -> Any:
    """Return the JSON-safe request field or None when absent."""
    if "request" not in details:
        return None
    return to_json_safe(details.get("request"))


def error_value(details: dict[str, Any], result: dict[str, Any]) -> Any:
    """Return the best available error value from event details/result data."""
    if "error" in details:
        return to_json_safe(details.get("error"))
    if "error" in result:
        return to_json_safe(result.get("error"))
    summary = result.get("summary")
    if isinstance(summary, dict) and "error" in summary:
        return to_json_safe(summary.get("error"))
    return None


def error_type_value(details: dict[str, Any], result: dict[str, Any]) -> Any:
    """Return the best available error type from event details/result data."""
    if "error_type" in details:
        return to_json_safe(details.get("error_type"))
    if "error_type" in result:
        return to_json_safe(result.get("error_type"))
    summary = result.get("summary")
    if isinstance(summary, dict) and "error_type" in summary:
        return to_json_safe(summary.get("error_type"))
    return None


def result_value(result: dict[str, Any], key: str) -> Any:
    """Return a JSON-safe result field, or None when it is absent."""
    if key not in result:
        return None
    return to_json_safe(result.get(key))


def _make_json_safe(value: Any) -> Any:
    """Convert arbitrary Python values into JSON-serializable values."""
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, dict):
        return {str(key): _make_json_safe(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_make_json_safe(item) for item in value]
    if isinstance(value, set):
        return [_make_json_safe(item) for item in sorted(value, key=repr)]

    model_dump = getattr(value, "model_dump", None)
    if callable(model_dump):
        return _make_json_safe(model_dump())

    try:
        json.dumps(value)
    except TypeError:
        return str(value)
    return value
