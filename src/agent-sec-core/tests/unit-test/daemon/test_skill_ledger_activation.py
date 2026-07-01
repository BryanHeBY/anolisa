"""Tests for Skill Ledger activation daemon integration."""

# ruff: noqa: I001

import asyncio
from pathlib import Path
from typing import Any

import pytest
from agent_sec_cli.daemon.errors import BadRequestError
from agent_sec_cli.daemon.protocol import DaemonRequest
from agent_sec_cli.daemon.runtime import DaemonRuntime
from agent_sec_cli.daemon.skill_ledger_activation import (
    METHOD_SKILLFS_NOTIFY_CHANGE,
    SKILL_LEDGER_ACTIVATION_JOB,
    SkillFsChange,
    SkillLedgerActivationJob,
    parse_skillfs_change,
    process_skill_change,
    skillfs_notify_change_handler,
)
from agent_sec_cli.skill_ledger.errors import SkillLedgerError


def make_skill(tmp_path: Path, name: str = "demo-skill") -> Path:
    """Create a minimal skill directory for daemon tests."""
    skill_dir = tmp_path / name
    skill_dir.mkdir()
    (skill_dir / "SKILL.md").write_text("# Demo Skill\n", encoding="utf-8")
    return skill_dir


def request_for(skill_dir: Path, **overrides: Any) -> DaemonRequest:
    """Build a daemon request for SkillFS notify tests."""
    params: dict[str, Any] = {
        "schemaVersion": 1,
        "skillDir": str(skill_dir),
        "skillName": skill_dir.name,
        "eventKind": "write",
        "paths": ["SKILL.md"],
    }
    params.update(overrides)
    return DaemonRequest(
        method=METHOD_SKILLFS_NOTIFY_CHANGE,
        params=params,
    )


def test_parse_skillfs_change_validates_request(tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")

    change = parse_skillfs_change(request_for(skill_dir).params)

    assert change.skill_dir == skill_dir.resolve()
    assert change.skill_name == "weather"
    assert change.event_kinds == {"write"}
    assert change.paths == {"SKILL.md"}


def test_parse_skillfs_change_accepts_reconcile_with_empty_paths(tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")

    change = parse_skillfs_change(
        request_for(skill_dir, eventKind="reconcile", paths=[]).params
    )

    assert change.skill_dir == skill_dir.resolve()
    assert change.skill_name == "weather"
    assert change.event_kinds == {"reconcile"}
    assert change.paths == set()


@pytest.mark.parametrize(
    ("overrides", "message"),
    [
        ({"schemaVersion": 2}, "schemaVersion"),
        ({"skillDir": "relative-skill"}, "absolute path"),
        ({"skillDir": "~/relative-to-home"}, "absolute path"),
        ({"skillName": "other"}, "skillName"),
        ({"eventKind": "chmod"}, "eventKind"),
        ({"paths": "/absolute"}, "paths"),
        ({"paths": ["/absolute"]}, "relative paths"),
        ({"paths": ["../escape"]}, "relative paths"),
        ({"paths": ["."]}, "relative paths"),
    ],
)
def test_parse_skillfs_change_rejects_invalid_params(
    tmp_path: Path,
    overrides: dict[str, Any],
    message: str,
):
    skill_dir = make_skill(tmp_path, "weather")

    with pytest.raises(BadRequestError, match=message):
        parse_skillfs_change(request_for(skill_dir, **overrides).params)


def test_parse_skillfs_change_requires_skill_manifest(tmp_path: Path):
    skill_dir = tmp_path / "not-a-skill"
    skill_dir.mkdir()

    with pytest.raises(BadRequestError, match="SKILL.md"):
        parse_skillfs_change(request_for(skill_dir).params)


def test_metadata_only_notification_is_accepted_and_ignored(tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")
    runtime = DaemonRuntime(socket_path=tmp_path / "daemon.sock")

    response = skillfs_notify_change_handler(
        request_for(skill_dir, paths=[".skill-meta/latest.json"]),
        runtime,
    )

    assert response.data["schemaVersion"] == 1
    assert response.data["accepted"] is True
    assert response.data["ignored"] is True
    assert response.data["reason"] == "metadata-only change"


def test_notify_enqueues_registered_activation_job(monkeypatch, tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")
    runtime = DaemonRuntime(socket_path=tmp_path / "daemon.sock")
    job = SkillLedgerActivationJob(debounce_seconds=0)
    runtime.jobs.register(job)
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_managed_skill_dirs",
        lambda: [],
    )

    async def scenario():
        await job.start()
        try:
            response = skillfs_notify_change_handler(request_for(skill_dir), runtime)
        finally:
            await job.stop()
        return response

    response = asyncio.run(scenario())

    assert response.data["schemaVersion"] == 1
    assert response.data["accepted"] is True
    assert response.data["ignored"] is False
    assert response.data["queued"] is True
    assert response.data["coalesced"] is False
    assert response.data["skill"]["skillName"] == "weather"


def test_notify_enqueues_reconcile_with_empty_paths(monkeypatch, tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")
    runtime = DaemonRuntime(socket_path=tmp_path / "daemon.sock")
    job = SkillLedgerActivationJob(debounce_seconds=0)
    runtime.jobs.register(job)
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_managed_skill_dirs",
        lambda: [],
    )

    async def scenario():
        await job.start()
        try:
            response = skillfs_notify_change_handler(
                request_for(skill_dir, eventKind="reconcile", paths=[]),
                runtime,
            )
        finally:
            await job.stop()
        return response

    response = asyncio.run(scenario())

    assert response.data["schemaVersion"] == 1
    assert response.data["accepted"] is True
    assert response.data["ignored"] is False
    assert response.data["queued"] is True
    assert response.data["coalesced"] is False
    assert response.data["skill"]["eventKinds"] == ["reconcile"]
    assert response.data["skill"]["paths"] == []


def test_activation_job_debounces_same_skill(monkeypatch, tmp_path: Path):
    skill_dir = make_skill(tmp_path, "weather")
    calls = []

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_managed_skill_dirs",
        lambda: [],
    )

    def fake_process(change: SkillFsChange) -> dict[str, Any]:
        calls.append(change)
        return {"status": "processed", "skill": change.to_dict()}

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation.process_skill_change",
        fake_process,
    )

    async def scenario():
        job = SkillLedgerActivationJob(debounce_seconds=0.05)
        await job.start()
        try:
            job.enqueue(
                SkillFsChange(
                    skill_dir=skill_dir.resolve(),
                    skill_name=skill_dir.name,
                    event_kinds={"write"},
                    paths={"SKILL.md"},
                )
            )
            job.enqueue(
                SkillFsChange(
                    skill_dir=skill_dir.resolve(),
                    skill_name=skill_dir.name,
                    event_kinds={"rename"},
                    paths={"scripts/run.sh"},
                )
            )
            job.enqueue(
                SkillFsChange(
                    skill_dir=skill_dir.resolve(),
                    skill_name=skill_dir.name,
                    event_kinds={"reconcile"},
                    paths=set(),
                )
            )
            deadline = asyncio.get_running_loop().time() + 1.0
            while len(calls) < 1 and asyncio.get_running_loop().time() < deadline:
                await asyncio.sleep(0.01)
        finally:
            await job.stop()

    asyncio.run(scenario())

    assert len(calls) == 1
    assert calls[0].event_kinds == {"write", "rename", "reconcile"}
    assert calls[0].paths == {"SKILL.md", "scripts/run.sh"}


def test_activation_job_debounces_events_arriving_during_drain(
    monkeypatch,
    tmp_path: Path,
):
    skill_dir = make_skill(tmp_path, "weather")
    calls: list[tuple[set[str], float]] = []

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_managed_skill_dirs",
        lambda: [],
    )

    async def scenario():
        job = SkillLedgerActivationJob(debounce_seconds=0.05)

        async def fake_process(change: SkillFsChange) -> None:
            calls.append((set(change.event_kinds), asyncio.get_running_loop().time()))
            if len(calls) == 1:
                job.enqueue(
                    SkillFsChange(
                        skill_dir=skill_dir.resolve(),
                        skill_name=skill_dir.name,
                        event_kinds={"rename"},
                        paths={"scripts/run.sh"},
                    )
                )

        monkeypatch.setattr(job, "_process_change", fake_process)
        await job.start()
        try:
            job.enqueue(
                SkillFsChange(
                    skill_dir=skill_dir.resolve(),
                    skill_name=skill_dir.name,
                    event_kinds={"write"},
                    paths={"SKILL.md"},
                )
            )
            deadline = asyncio.get_running_loop().time() + 1.0
            while len(calls) < 2 and asyncio.get_running_loop().time() < deadline:
                await asyncio.sleep(0.01)
        finally:
            await job.stop()

    asyncio.run(scenario())

    assert [event_kinds for event_kinds, _ in calls] == [{"write"}, {"rename"}]
    assert calls[1][1] - calls[0][1] >= 0.04


def test_drain_pending_requeues_batch_on_cancelled_process(
    monkeypatch,
    tmp_path: Path,
):
    first = make_skill(tmp_path, "weather")
    second = make_skill(tmp_path, "calendar")

    async def scenario():
        job = SkillLedgerActivationJob(debounce_seconds=0)
        job._wake_event = asyncio.Event()
        changes = [
            SkillFsChange(
                skill_dir=first.resolve(),
                skill_name=first.name,
                event_kinds={"write"},
                paths={"SKILL.md"},
            ),
            SkillFsChange(
                skill_dir=second.resolve(),
                skill_name=second.name,
                event_kinds={"write"},
                paths={"SKILL.md"},
            ),
        ]
        job._pending = {change.skill_dir: change for change in changes}

        async def fail_process(_change: SkillFsChange) -> None:
            raise asyncio.CancelledError()

        monkeypatch.setattr(job, "_process_change", fail_process)
        with pytest.raises(asyncio.CancelledError):
            await job._drain_pending()
        return job._pending

    pending = asyncio.run(scenario())

    assert set(pending) == {first.resolve(), second.resolve()}


def test_process_skill_change_resolves_activation_after_scan_error(
    monkeypatch,
    tmp_path: Path,
):
    skill_dir = make_skill(tmp_path, "weather")
    backend = object()
    events = []

    def fake_backend() -> object:
        return backend

    def fail_scan(path: str, received_backend: object) -> dict[str, Any]:
        events.append(("scan", path, received_backend))
        raise RuntimeError("scanner failed")

    def fake_policy() -> str:
        return "pass_only"

    def fake_resolve(
        path: str,
        received_backend: object,
        policy: str,
    ) -> dict[str, Any]:
        events.append(("resolve", path, received_backend, policy))
        return {"target": None}

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._ensure_default_backend",
        fake_backend,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._scan_skill",
        fail_scan,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_activation",
        fake_resolve,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_activation_policy",
        fake_policy,
    )

    result = process_skill_change(
        SkillFsChange(
            skill_dir=skill_dir.resolve(),
            skill_name=skill_dir.name,
            event_kinds={"write"},
            paths={"SKILL.md"},
        )
    )

    assert result["status"] == "error"
    assert result["error"] == "scanner failed"
    assert result["activation"] == {"target": None}
    assert events == [
        ("scan", str(skill_dir.resolve()), backend),
        ("resolve", str(skill_dir.resolve()), backend, "pass_only"),
    ]


def test_process_skill_change_skips_unresolved_live_root_from_scan(
    monkeypatch,
    tmp_path: Path,
):
    skill_dir = make_skill(tmp_path, "weather")
    backend = object()
    message = (
        f"cannot resolve live skill root for {skill_dir.resolve()}; the path may be "
        "a SkillFS runtime view. Run the command against a Skill Ledger managed "
        "source/backing skill path or ensure managedSkillDirs points to "
        "source/backing roots."
    )

    def fake_backend() -> object:
        return backend

    def fail_scan(path: str, received_backend: object) -> dict[str, Any]:
        assert path == str(skill_dir.resolve())
        assert received_backend is backend
        raise SkillLedgerError(message)

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._ensure_default_backend",
        fake_backend,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._scan_skill",
        fail_scan,
    )

    result = process_skill_change(
        SkillFsChange(
            skill_dir=skill_dir.resolve(),
            skill_name=skill_dir.name,
            event_kinds={"write"},
            paths={"SKILL.md"},
        )
    )

    assert result["status"] == "skipped"
    assert result["reasonCode"] == "unmanaged_skill_root"
    assert result["message"] == message
    assert result["skill"]["skillName"] == "weather"
    assert result["scan"] is None
    assert result["activation"] is None
    assert "error" not in result


def test_process_skill_change_skips_unresolved_live_root_from_activation(
    monkeypatch,
    tmp_path: Path,
):
    skill_dir = make_skill(tmp_path, "weather")
    backend = object()
    message = (
        f"cannot resolve live skill root for {skill_dir.resolve()}; the path may be "
        "a SkillFS runtime view. Run the command against a Skill Ledger managed "
        "source/backing skill path or ensure managedSkillDirs points to "
        "source/backing roots."
    )

    def fake_backend() -> object:
        return backend

    def fake_scan(path: str, received_backend: object) -> dict[str, Any]:
        assert path == str(skill_dir.resolve())
        assert received_backend is backend
        return {"status": "noop"}

    def fail_resolve(
        path: str, received_backend: object, policy: str
    ) -> dict[str, Any]:
        assert path == str(skill_dir.resolve())
        assert received_backend is backend
        assert policy == "pass_only"
        raise SkillLedgerError(message)

    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._ensure_default_backend",
        fake_backend,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._scan_skill",
        fake_scan,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_activation",
        fail_resolve,
    )
    monkeypatch.setattr(
        "agent_sec_cli.daemon.skill_ledger_activation._resolve_activation_policy",
        lambda: "pass_only",
    )

    result = process_skill_change(
        SkillFsChange(
            skill_dir=skill_dir.resolve(),
            skill_name=skill_dir.name,
            event_kinds={"write"},
            paths={"SKILL.md"},
        )
    )

    assert result["status"] == "skipped"
    assert result["reasonCode"] == "unmanaged_skill_root"
    assert result["message"] == message
    assert result["skill"]["skillName"] == "weather"
    assert result["scan"] is None
    assert result["activation"] is None
    assert "error" not in result


def test_default_job_name_is_stable():
    assert SkillLedgerActivationJob().name == SKILL_LEDGER_ACTIVATION_JOB
