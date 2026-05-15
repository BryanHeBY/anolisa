"""Capability registry — config loading, safe wrapping, and registration."""

from __future__ import annotations

import logging
import time
import tomllib
from pathlib import Path
from typing import Any

logger = logging.getLogger("agent-sec-core")

# If a single hook invocation exceeds this threshold (seconds), emit a warning.
_SLOW_HOOK_THRESHOLD = 2.0


def load_config(plugin_dir: Path) -> dict[str, Any]:
    """Load config.toml from the plugin directory.

    Returns an empty dict on any failure (fail-open).
    """
    config_path = plugin_dir / "config.toml"
    try:
        with open(config_path, "rb") as f:
            return tomllib.load(f)
    except (FileNotFoundError, tomllib.TOMLDecodeError, OSError) as e:
        logger.warning(f"Failed to load config: {e}")
        return {}


def safe_hook_wrapper(callback, capability_id: str):
    """Wrap a hook callback with try/except and performance logging.

    - Catches all exceptions → logs and returns None (fail-open)
    - Logs a warning when execution exceeds _SLOW_HOOK_THRESHOLD
    """

    def wrapper(*args, **kwargs):
        start = time.monotonic()
        try:
            result = callback(*args, **kwargs)
        except Exception as e:
            logger.error(f"[{capability_id}] hook error: {e}")
            return None
        elapsed = time.monotonic() - start
        if elapsed > _SLOW_HOOK_THRESHOLD:
            logger.warning(f"[{capability_id}] slow hook: {elapsed:.2f}s")
        return result

    return wrapper


def register_capabilities(ctx, capabilities: list, config: dict) -> None:
    """Register all enabled capabilities with the Hermes plugin context."""
    caps_config = config.get("capabilities", {})

    for cap in capabilities:
        cap_config = caps_config.get(cap.id, {})
        if not cap_config.get("enabled", True):
            logger.info(f"[{cap.id}] disabled by config, skipping")
            continue
        try:
            cap.register(ctx, cap_config)
            logger.info(f"[{cap.id}] registered successfully")
        except Exception as e:
            logger.error(f"[{cap.id}] registration failed: {e}")
