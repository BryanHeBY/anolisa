"""Tests for Skill Ledger worker process lifecycle management."""

# ruff: noqa: I001

import asyncio
from pathlib import Path

import pytest
from agent_sec_cli.daemon.jobs.skill_ledger import (
    worker_client as worker_client_module,
)
from agent_sec_cli.daemon.jobs.skill_ledger.protocol import (
    SkillFsChange,
    error_worker_response,
    parse_worker_request,
    serialize_worker_response,
    success_worker_response,
)
from agent_sec_cli.daemon.jobs.skill_ledger.worker_client import (
    SkillLedgerWorkerClient,
    SkillLedgerWorkerExecutionError,
    SkillLedgerWorkerTransportError,
)


class FakeReader:
    def __init__(self) -> None:
        self._queue: asyncio.Queue[bytes] = asyncio.Queue()

    async def readline(self) -> bytes:
        return await self._queue.get()

    async def read(self, _size: int) -> bytes:
        return b""

    def feed(self, data: bytes) -> None:
        self._queue.put_nowait(data)


class FakeStdin:
    def __init__(self, process: "FakeProcess") -> None:
        self._process = process
        self.closed = False
        self.written = asyncio.Event()

    def write(self, data: bytes) -> None:
        request = parse_worker_request(data)
        self.written.set()
        if self._process.behavior == "success":
            response = success_worker_response(
                request.request_id,
                {"status": "processed", "workerPid": self._process.pid},
            )
            self._process.stdout.feed(serialize_worker_response(response))
        elif self._process.behavior == "scan_error":
            response = success_worker_response(
                request.request_id,
                {
                    "status": "error",
                    "error": "scan failed",
                    "workerPid": self._process.pid,
                },
            )
            self._process.stdout.feed(serialize_worker_response(response))
        elif self._process.behavior == "execution_error":
            response = error_worker_response(
                request.request_id,
                RuntimeError("scan failed"),
            )
            self._process.stdout.feed(serialize_worker_response(response))
        elif self._process.behavior == "invalid":
            self._process.stdout.feed(b"{}\n")
        elif self._process.behavior == "eof":
            self._process.finish(1)
            self._process.stdout.feed(b"")

    async def drain(self) -> None:
        pass

    def close(self) -> None:
        self.closed = True
        if self._process.exit_on_close:
            self._process.finish(0)

    async def wait_closed(self) -> None:
        pass


class FakeProcess:
    def __init__(
        self,
        pid: int,
        behavior: str,
        *,
        exit_on_close: bool = True,
        exit_on_terminate: bool = True,
    ) -> None:
        self.pid = pid
        self.behavior = behavior
        self.exit_on_close = exit_on_close
        self.exit_on_terminate = exit_on_terminate
        self.returncode = None
        self.stdout = FakeReader()
        self.stderr = FakeReader()
        self.stdin = FakeStdin(self)
        self.signals: list[str] = []
        self._exited = asyncio.Event()

    async def wait(self) -> int:
        await self._exited.wait()
        assert self.returncode is not None
        return self.returncode

    def terminate(self) -> None:
        self.signals.append("terminate")
        if self.exit_on_terminate:
            self.finish(-15)

    def kill(self) -> None:
        self.signals.append("kill")
        self.finish(-9)

    def finish(self, returncode: int) -> None:
        if self.returncode is None:
            self.returncode = returncode
            self._exited.set()


def make_change(tmp_path: Path) -> SkillFsChange:
    skill_dir = tmp_path / "weather"
    return SkillFsChange(
        skill_dir=skill_dir,
        skill_name=skill_dir.name,
        event_kinds={"write"},
        paths={"SKILL.md"},
    )


def install_process_factory(monkeypatch, processes: list[FakeProcess]) -> list[tuple]:
    calls = []

    async def create_process(*args, **kwargs):
        calls.append((args, kwargs))
        return processes.pop(0)

    monkeypatch.setattr(asyncio, "create_subprocess_exec", create_process)
    return calls


def test_worker_client_starts_lazily_and_reuses_process(monkeypatch, tmp_path: Path):
    process = FakeProcess(101, "success")
    calls = install_process_factory(monkeypatch, [process])

    async def scenario():
        client = SkillLedgerWorkerClient()
        assert client.pid is None
        first = await client.process_change(make_change(tmp_path))
        second = await client.process_change(make_change(tmp_path))
        pid = client.pid
        await client.stop()
        return first, second, pid

    first, second, pid = asyncio.run(scenario())

    assert len(calls) == 1
    assert calls[0][0][:3] == (
        worker_client_module.sys.executable,
        "-m",
        "agent_sec_cli.daemon.jobs.skill_ledger.worker",
    )
    assert first["workerPid"] == 101
    assert second["workerPid"] == 101
    assert pid == 101


@pytest.mark.parametrize("first_behavior", ["eof", "invalid"])
def test_worker_client_restarts_once_and_retries_current_change(
    monkeypatch,
    tmp_path: Path,
    first_behavior: str,
):
    first = FakeProcess(
        101,
        first_behavior,
        exit_on_close=first_behavior != "invalid",
    )
    second = FakeProcess(102, "success")
    calls = install_process_factory(monkeypatch, [first, second])

    async def scenario():
        client = SkillLedgerWorkerClient()
        result = await client.process_change(make_change(tmp_path))
        pid = client.pid
        await client.stop()
        return result, pid

    result, pid = asyncio.run(scenario())

    assert len(calls) == 2
    assert result["workerPid"] == 102
    assert pid == 102
    if first_behavior == "invalid":
        assert first.signals == ["terminate"]


def test_worker_execution_error_does_not_restart(monkeypatch, tmp_path: Path):
    process = FakeProcess(101, "execution_error")
    calls = install_process_factory(monkeypatch, [process])

    async def scenario():
        client = SkillLedgerWorkerClient()
        with pytest.raises(SkillLedgerWorkerExecutionError, match="scan failed"):
            await client.process_change(make_change(tmp_path))
        pid = client.pid
        await client.stop()
        return pid

    pid = asyncio.run(scenario())

    assert len(calls) == 1
    assert pid == 101


def test_worker_transport_retries_only_once(monkeypatch, tmp_path: Path):
    calls = install_process_factory(
        monkeypatch,
        [FakeProcess(101, "eof"), FakeProcess(102, "eof")],
    )

    async def scenario():
        client = SkillLedgerWorkerClient()
        with pytest.raises(SkillLedgerWorkerTransportError, match="exit code 1"):
            await client.process_change(make_change(tmp_path))
        return client.pid

    pid = asyncio.run(scenario())

    assert len(calls) == 2
    assert pid is None


def test_worker_scan_error_does_not_restart(monkeypatch, tmp_path: Path):
    process = FakeProcess(101, "scan_error")
    calls = install_process_factory(monkeypatch, [process])

    async def scenario():
        client = SkillLedgerWorkerClient()
        result = await client.process_change(make_change(tmp_path))
        pid = client.pid
        await client.stop()
        return result, pid

    result, pid = asyncio.run(scenario())

    assert len(calls) == 1
    assert result["error"] == "scan failed"
    assert pid == 101


def test_worker_cancellation_terminates_process(monkeypatch, tmp_path: Path):
    process = FakeProcess(
        101,
        "hang",
        exit_on_close=False,
        exit_on_terminate=True,
    )
    install_process_factory(monkeypatch, [process])

    async def scenario():
        client = SkillLedgerWorkerClient()
        task = asyncio.create_task(client.process_change(make_change(tmp_path)))
        await process.stdin.written.wait()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return client.pid

    pid = asyncio.run(scenario())

    assert process.signals == ["terminate"]
    assert pid is None


def test_worker_stop_escalates_to_kill(monkeypatch, tmp_path: Path):
    process = FakeProcess(
        101,
        "success",
        exit_on_close=False,
        exit_on_terminate=False,
    )
    install_process_factory(monkeypatch, [process])
    monkeypatch.setattr(
        worker_client_module,
        "_GRACEFUL_SHUTDOWN_TIMEOUT_SECONDS",
        0.01,
    )
    monkeypatch.setattr(
        worker_client_module,
        "_TERMINATE_TIMEOUT_SECONDS",
        0.01,
    )

    async def scenario():
        client = SkillLedgerWorkerClient()
        await client.process_change(make_change(tmp_path))
        await client.stop()

    asyncio.run(scenario())

    assert process.stdin.closed is True
    assert process.signals == ["terminate", "kill"]
