"""Skill Ledger activation job and SkillFS notification handler."""

import asyncio
import contextlib
from pathlib import Path
from typing import Any

from agent_sec_cli.daemon.errors import BadRequestError, UnavailableError
from agent_sec_cli.daemon.jobs.base import BackgroundJob, JobStatus, utc_now
from agent_sec_cli.daemon.jobs.skill_ledger.processor import (
    process_skill_change,
)
from agent_sec_cli.daemon.jobs.skill_ledger.protocol import SkillFsChange
from agent_sec_cli.daemon.protocol import DaemonRequest
from agent_sec_cli.daemon.registry import HandlerResult, MethodSpec
from agent_sec_cli.daemon.runtime import DaemonRuntime

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


def _resolve_managed_skill_dirs() -> list[Path]:
    from agent_sec_cli.skill_ledger.config import (  # noqa: PLC0415
        resolve_managed_skill_dirs,
    )

    return resolve_managed_skill_dirs()
