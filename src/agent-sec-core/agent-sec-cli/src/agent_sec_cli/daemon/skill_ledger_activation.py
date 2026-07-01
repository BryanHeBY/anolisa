"""Skill Ledger activation daemon job and SkillFS notification handler.

This module intentionally keeps top-level imports light. Skill Ledger modules
are imported inside worker paths so daemon health/registry construction stays
cheap and does not initialize scanner or signing machinery.
"""

import asyncio
import contextlib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from agent_sec_cli.daemon.errors import BadRequestError, UnavailableError
from agent_sec_cli.daemon.jobs.base import BackgroundJob, JobStatus, utc_now
from agent_sec_cli.daemon.protocol import DaemonRequest
from agent_sec_cli.daemon.registry import HandlerResult, MethodSpec
from agent_sec_cli.daemon.runtime import DaemonRuntime
from agent_sec_cli.skill_ledger.errors import SkillLedgerError

METHOD_SKILLFS_NOTIFY_CHANGE = "skill_ledger.skillfs_notify_change"
SCHEMA_VERSION = 1
SKILL_LEDGER_ACTIVATION_JOB = "skill-ledger-activation"
DEFAULT_DEBOUNCE_SECONDS = 0.5

_SKILL_MANIFEST = "SKILL.md"
_SKILL_META = ".skill-meta"
_ALLOWED_EVENT_KINDS = frozenset(
    {
        "mkdir",
        "create",
        "write",
        "rename",
        "unlink",
        "rmdir",
        "setattr",
        "truncate",
        "reconcile",
    }
)
_UNRESOLVED_LIVE_ROOT_PREFIX = "cannot resolve live skill root for "


@dataclass
class SkillFsChange:
    """Validated SkillFS change notification."""

    skill_dir: Path
    skill_name: str
    event_kinds: set[str] = field(default_factory=set)
    paths: set[str] = field(default_factory=set)

    def merge(self, other: "SkillFsChange") -> None:
        """Merge another notification for the same skill."""
        self.event_kinds.update(other.event_kinds)
        self.paths.update(other.paths)

    def to_dict(self) -> dict[str, Any]:
        """Return a JSON-serializable job/debug payload."""
        return {
            "skillDir": str(self.skill_dir),
            "skillName": self.skill_name,
            "eventKinds": sorted(self.event_kinds),
            "paths": sorted(self.paths),
        }


class SkillLedgerActivationJob(BackgroundJob):
    """Debounced Skill Ledger scanner/activation worker."""

    name = SKILL_LEDGER_ACTIVATION_JOB

    def __init__(self, debounce_seconds: float = DEFAULT_DEBOUNCE_SECONDS) -> None:
        if debounce_seconds < 0:
            raise ValueError("debounce_seconds must be non-negative")
        self.debounce_seconds = debounce_seconds
        self._task: asyncio.Task[None] | None = None
        self._wake_event: asyncio.Event | None = None
        self._pending: dict[Path, SkillFsChange] = {}
        self._state = "stopped"
        self._last_error: str | None = None
        self._last_tick_at: str | None = None
        self._last_processed: dict[str, Any] | None = None

    async def start(self) -> None:
        """Start the activation worker and enqueue startup reconciliation."""
        if self._task is not None and not self._task.done():
            return
        self._wake_event = asyncio.Event()
        self._state = "running"
        self._task = asyncio.create_task(self._run_loop())
        self._enqueue_reconcile()

    async def stop(self) -> None:
        """Stop the activation worker."""
        if self._task is not None:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
            self._task = None
        self._wake_event = None
        self._state = "stopped"

    def status(self) -> JobStatus:
        """Return current job status."""
        return JobStatus(
            name=self.name,
            state=self._state,
            last_error=self._last_error,
            last_tick_at=self._last_tick_at,
        )

    def enqueue(self, change: SkillFsChange) -> bool:
        """Queue a SkillFS change. Returns whether it was newly queued."""
        if self._wake_event is None:
            raise UnavailableError("skill-ledger activation job is not running")
        existing = self._pending.get(change.skill_dir)
        newly_queued = existing is None
        if existing is None:
            self._pending[change.skill_dir] = change
        else:
            existing.merge(change)
        self._wake_event.set()
        return newly_queued

    @property
    def last_processed(self) -> dict[str, Any] | None:
        """Return the last processed result for tests and diagnostics."""
        return self._last_processed

    async def _run_loop(self) -> None:
        while True:
            if self._wake_event is None:
                return
            await self._wake_event.wait()
            self._wake_event.clear()
            if self.debounce_seconds:
                await asyncio.sleep(self.debounce_seconds)
            await self._drain_pending()

    async def _drain_pending(self) -> None:
        pending = self._pending
        self._pending = {}
        changes = list(pending.values())
        for index, change in enumerate(changes):
            try:
                await self._process_change(change)
            except asyncio.CancelledError:
                self._requeue_changes(changes[index:])
                raise

    def _requeue_changes(self, changes: list[SkillFsChange]) -> None:
        for change in changes:
            existing = self._pending.get(change.skill_dir)
            if existing is None:
                self._pending[change.skill_dir] = change
            else:
                existing.merge(change)
        if changes and self._wake_event is not None:
            self._wake_event.set()

    async def _process_change(self, change: SkillFsChange) -> None:
        self._last_tick_at = utc_now()
        try:
            result = await asyncio.to_thread(process_skill_change, change)
            self._last_processed = result
            self._last_error = result.get("error")
            self._state = "error" if self._last_error else "running"
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            self._last_error = str(exc)
            self._last_processed = {
                "skillDir": str(change.skill_dir),
                "skillName": change.skill_name,
                "status": "error",
                "error": str(exc),
            }
            self._state = "error"

    def _enqueue_reconcile(self) -> None:
        try:
            for skill_dir in _resolve_managed_skill_dirs():
                self.enqueue(
                    SkillFsChange(
                        skill_dir=skill_dir.resolve(),
                        skill_name=skill_dir.name,
                        event_kinds={"reconcile"},
                        paths=set(),
                    )
                )
        except Exception as exc:
            self._last_error = str(exc)
            self._state = "error"


def skillfs_notify_change_handler(
    request: DaemonRequest,
    runtime: DaemonRuntime,
) -> HandlerResult:
    """Validate a SkillFS change notification and enqueue daemon processing."""
    change = parse_skillfs_change(request.params)
    if _paths_are_metadata_only(change.paths):
        return HandlerResult(
            data={
                "schemaVersion": SCHEMA_VERSION,
                "accepted": True,
                "ignored": True,
                "reason": "metadata-only change",
                "skill": change.to_dict(),
            }
        )

    job = runtime.jobs.get(SKILL_LEDGER_ACTIVATION_JOB)
    if job is None or not hasattr(job, "enqueue"):
        raise UnavailableError("skill-ledger activation job is not registered")
    newly_queued = job.enqueue(change)
    return HandlerResult(
        data={
            "schemaVersion": SCHEMA_VERSION,
            "accepted": True,
            "ignored": False,
            "queued": True,
            "coalesced": not newly_queued,
            "skill": change.to_dict(),
        }
    )


def skillfs_notify_method_spec() -> MethodSpec:
    """Return the daemon method spec for SkillFS change notifications."""
    return MethodSpec(
        method=METHOD_SKILLFS_NOTIFY_CHANGE,
        handler=skillfs_notify_change_handler,
        lifecycle="skill_ledger",
        queue="skill_ledger",
        timeout_ms=1000,
        access_log=True,
    )


def parse_skillfs_change(params: dict[str, Any]) -> SkillFsChange:
    """Validate daemon request params for a SkillFS change notification."""
    schema_version = params.get("schemaVersion")
    if schema_version != SCHEMA_VERSION:
        raise BadRequestError("params.schemaVersion must be 1")

    skill_dir = _validate_skill_dir(params.get("skillDir"))
    skill_name = params.get("skillName")
    if not isinstance(skill_name, str) or not skill_name:
        raise BadRequestError("params.skillName must be a non-empty string")
    if skill_name != skill_dir.name:
        raise BadRequestError("params.skillName must match skillDir basename")

    event_kind = params.get("eventKind")
    if event_kind not in _ALLOWED_EVENT_KINDS:
        allowed = ", ".join(sorted(_ALLOWED_EVENT_KINDS))
        raise BadRequestError(f"params.eventKind must be one of: {allowed}")

    paths = _validate_paths(params.get("paths"))
    return SkillFsChange(
        skill_dir=skill_dir,
        skill_name=skill_name,
        event_kinds={event_kind},
        paths=set(paths),
    )


def process_skill_change(change: SkillFsChange) -> dict[str, Any]:
    """Run scan and activation resolution for a debounced SkillFS change."""
    backend = _ensure_default_backend()
    policy = _resolve_activation_policy()
    scan_result: dict[str, Any] | None = None
    scan_error: str | None = None
    try:
        scan_result = _scan_skill(str(change.skill_dir), backend)
    except Exception as exc:
        if _is_unresolved_live_root_error(exc):
            return _skipped_unmanaged_result(change, str(exc))
        scan_error = str(exc)

    try:
        activation_result = _resolve_activation(str(change.skill_dir), backend, policy)
    except Exception as exc:
        if scan_error is None and _is_unresolved_live_root_error(exc):
            return _skipped_unmanaged_result(change, str(exc))
        raise
    result: dict[str, Any] = {
        "status": "processed" if scan_error is None else "error",
        "skill": change.to_dict(),
        "scan": scan_result,
        "activation": activation_result,
    }
    if scan_error is not None:
        result["error"] = scan_error
    return result


def _is_unresolved_live_root_error(exc: Exception) -> bool:
    return isinstance(exc, SkillLedgerError) and str(exc).startswith(
        _UNRESOLVED_LIVE_ROOT_PREFIX
    )


def _skipped_unmanaged_result(change: SkillFsChange, message: str) -> dict[str, Any]:
    return {
        "status": "skipped",
        "reasonCode": "unmanaged_skill_root",
        "message": message,
        "skill": change.to_dict(),
        "scan": None,
        "activation": None,
    }


def _validate_skill_dir(value: Any) -> Path:
    if not isinstance(value, str) or not value:
        raise BadRequestError("params.skillDir must be a non-empty string")
    path = Path(value)
    if not path.is_absolute():
        raise BadRequestError("params.skillDir must be an absolute path")
    try:
        resolved = path.resolve(strict=True)
    except OSError as exc:
        raise BadRequestError(f"params.skillDir is not accessible: {exc}") from exc
    if not resolved.is_dir():
        raise BadRequestError("params.skillDir must be a directory")
    if not (resolved / _SKILL_MANIFEST).is_file():
        raise BadRequestError("params.skillDir must contain SKILL.md")
    return resolved


def _validate_paths(value: Any) -> list[str]:
    if not isinstance(value, list):
        raise BadRequestError("params.paths must be a list")
    paths: list[str] = []
    for item in value:
        if not isinstance(item, str) or not item:
            raise BadRequestError("params.paths must contain non-empty strings")
        path = Path(item)
        if not path.parts or path.is_absolute() or ".." in path.parts:
            raise BadRequestError("params.paths must be relative paths under skillDir")
        paths.append(item)
    return paths


def _paths_are_metadata_only(paths: set[str]) -> bool:
    return bool(paths) and all(Path(path).parts[0] == _SKILL_META for path in paths)


def _ensure_default_backend() -> Any:
    from agent_sec_cli.skill_ledger.signing.ed25519 import (  # noqa: PLC0415
        NativeEd25519Backend,
    )
    from agent_sec_cli.skill_ledger.signing.key_manager import (  # noqa: PLC0415
        keys_exist,
    )

    if not keys_exist():
        NativeEd25519Backend().generate_keys(passphrase=None)
    return NativeEd25519Backend()


def _scan_skill(skill_dir: str, backend: Any) -> dict[str, Any]:
    from agent_sec_cli.skill_ledger.core.certifier import (  # noqa: PLC0415
        scan_skill,
    )

    return scan_skill(skill_dir, backend, force=False)


def _resolve_activation(skill_dir: str, backend: Any, policy: str) -> dict[str, Any]:
    from agent_sec_cli.skill_ledger.core.resolver import (  # noqa: PLC0415
        resolve_activation,
    )

    return resolve_activation(skill_dir, backend, policy=policy)


def _resolve_activation_policy() -> str:
    from agent_sec_cli.skill_ledger.config import (  # noqa: PLC0415
        resolve_activation_policy,
    )

    return resolve_activation_policy()


def _resolve_managed_skill_dirs() -> list[Path]:
    from agent_sec_cli.skill_ledger.config import (  # noqa: PLC0415
        resolve_managed_skill_dirs,
    )

    return resolve_managed_skill_dirs()
