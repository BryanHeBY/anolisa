"""Detector for user-defined PII regex rules."""

import logging
import time
from pathlib import Path

from agent_sec_cli.pii_checker.custom_rules import (
    CustomRuleStatus,
    load_custom_rules,
)
from agent_sec_cli.pii_checker.detectors.base import PiiCandidate
from agent_sec_cli.pii_checker.models import PiiCategory, PiiSeverity

RULE_TIMEOUT_SECONDS = 0.020
SCAN_BUDGET_SECONDS = 0.200
MAX_CUSTOM_FINDINGS = 100
_LOGGER = logging.getLogger(__name__)


class CustomPiiDetector:
    """Run validated custom regex rules within bounded scan time."""

    name = "custom_rule"
    engine = "regex"

    def __init__(self, rules_path: Path | None = None) -> None:
        """Create a detector using the fixed path unless tests inject one."""
        self._rules_path = rules_path
        self._summary = self._new_summary(CustomRuleStatus.ABSENT.value, 0)

    def detect(self, text: str) -> list[PiiCandidate]:
        """Return candidates from custom rules while enforcing runtime limits."""
        try:
            ruleset = load_custom_rules(self._rules_path)
        except Exception as exc:  # noqa: BLE001 - custom rules must fail open
            self._summary = self._new_summary(CustomRuleStatus.INVALID.value, 0)
            self._summary["runtime_error_count"] = 1
            self._summary["error_code"] = "load_error"
            _LOGGER.warning(
                "PII custom rules load error: %s",
                type(exc).__name__,
            )
            return []
        self._summary = self._new_summary(ruleset.status.value, len(ruleset.rules))
        if ruleset.ruleset_sha256 is not None:
            self._summary["ruleset_sha256"] = ruleset.ruleset_sha256
        if ruleset.error_code is not None:
            self._summary["error_code"] = ruleset.error_code
        if ruleset.status is not CustomRuleStatus.LOADED:
            return []

        candidates: list[PiiCandidate] = []
        deadline = time.perf_counter() + SCAN_BUDGET_SECONDS
        ordered_rules = sorted(
            ruleset.rules,
            key=lambda rule: rule.severity != PiiSeverity.DENY.value,
        )
        for rule in ordered_rules:
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                self._summary["budget_exhausted"] = True
                break

            timeout = min(RULE_TIMEOUT_SECONDS, remaining)
            try:
                matches = rule.pattern.finditer(text, timeout=timeout)
                for match in matches:
                    if time.perf_counter() >= deadline:
                        self._summary["budget_exhausted"] = True
                        break
                    start, end = match.span()
                    if start == end:
                        self._summary["runtime_error_count"] += 1
                        continue
                    if len(candidates) >= MAX_CUSTOM_FINDINGS:
                        self._summary["truncated"] = True
                        break
                    candidates.append(
                        PiiCandidate(
                            pii_type=rule.pii_type,
                            category=PiiCategory.CUSTOM.value,
                            severity=rule.severity,
                            confidence=1.0,
                            value=match.group(0),
                            span=(start, end),
                            detector=self.name,
                            engine=self.engine,
                        )
                    )
            except TimeoutError:
                self._summary["runtime_error_count"] += 1
            except Exception as exc:  # noqa: BLE001 - custom rules must fail open
                self._summary["runtime_error_count"] += 1
                _LOGGER.warning(
                    "PII custom rule runtime error for type %s: %s",
                    rule.pii_type,
                    type(exc).__name__,
                )

            if time.perf_counter() >= deadline:
                self._summary["budget_exhausted"] = True

            if self._summary["budget_exhausted"]:
                break

        return candidates

    def summary(self) -> dict[str, object]:
        """Return sanitized configuration and runtime status for this scan."""
        return dict(self._summary)

    @staticmethod
    def _new_summary(status: str, rule_count: int) -> dict[str, object]:
        """Build a fresh sanitized runtime summary."""
        return {
            "status": status,
            "rule_count": rule_count,
            "runtime_error_count": 0,
            "budget_exhausted": False,
            "truncated": False,
        }
