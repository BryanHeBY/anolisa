"""Stdlib Unix socket client for the agent-sec daemon."""

import socket
from pathlib import Path
from typing import Any

from agent_sec_cli.correlation_context import (
    clean_correlation_value,
    get_current_trace_context,
    get_invocation_id,
    trace_context_to_payload,
)
from agent_sec_cli.daemon.errors import (
    DaemonClientTimeoutError,
    DaemonProtocolError,
    DaemonTransportError,
)
from agent_sec_cli.daemon.protocol import (
    DEFAULT_MAX_RESPONSE_BYTES,
    DEFAULT_TIMEOUT_MS,
    DaemonRequest,
    DaemonResponse,
    parse_response_line,
    serialize_request,
)
from agent_sec_cli.daemon.runtime import resolve_socket_path


class DaemonClient:
    """Synchronous Unix socket daemon client."""

    def __init__(
        self,
        socket_path: str | Path | None = None,
        timeout_ms: int = DEFAULT_TIMEOUT_MS,
        max_response_bytes: int = DEFAULT_MAX_RESPONSE_BYTES,
    ) -> None:
        self.socket_path = resolve_socket_path(socket_path)
        self.timeout_ms = timeout_ms
        self.max_response_bytes = max_response_bytes

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        trace_context: dict[str, Any] | None = None,
        timeout_ms: int | None = None,
        caller: str | None = None,
    ) -> DaemonResponse:
        """Send one request and return the daemon response.

        When *trace_context* is ``None`` (the default), the current
        process-level trace context is automatically inherited.  Pass
        an explicit empty dict ``{}`` to suppress inheritance (e.g. for
        health checks).
        """
        effective_timeout_ms = timeout_ms or self.timeout_ms
        if trace_context is None:
            trace_context = trace_context_to_payload(get_current_trace_context())
        request = DaemonRequest(
            method=method,
            params={} if params is None else params,
            trace_context=_trace_context_with_fallback_trace_id(trace_context),
            caller=caller,
            timeout_ms=effective_timeout_ms,
        )
        return self._send_request(request, effective_timeout_ms)

    def _send_request(self, request: DaemonRequest, timeout_ms: int) -> DaemonResponse:
        timeout_seconds = timeout_ms / 1000
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client_socket:
                client_socket.settimeout(timeout_seconds)
                client_socket.connect(str(self.socket_path))
                client_socket.sendall(serialize_request(request))
                response_line = self._read_response_line(client_socket)
        except socket.timeout as exc:
            raise DaemonClientTimeoutError("daemon request timed out") from exc
        except OSError as exc:
            raise DaemonTransportError(f"daemon is unavailable: {exc}") from exc

        try:
            return parse_response_line(response_line)
        except Exception as exc:
            raise DaemonProtocolError("daemon returned an invalid response") from exc

    def _read_response_line(self, client_socket: socket.socket) -> bytes:
        chunks: list[bytes] = []
        total_bytes = 0

        while True:
            chunk = client_socket.recv(4096)
            if not chunk:
                break

            chunks.append(chunk)
            total_bytes += len(chunk)
            if total_bytes > self.max_response_bytes:
                raise DaemonProtocolError("daemon response exceeds byte limit")
            if b"\n" in chunk:
                break

        if not chunks:
            raise DaemonTransportError("daemon returned an empty response")

        raw_response = b"".join(chunks)
        response_line, _separator, _remaining = raw_response.partition(b"\n")
        return response_line


def daemon_health_reachable(socket_path: Path, timeout_ms: int = 250) -> bool:
    """Return whether daemon.health can be reached at a socket path."""
    try:
        response = DaemonClient(socket_path=socket_path, timeout_ms=timeout_ms).call(
            "daemon.health",
            timeout_ms=timeout_ms,
            trace_context={},
        )
    except (DaemonProtocolError, DaemonTransportError):
        return False
    return response.ok


def _trace_context_with_fallback_trace_id(
    trace_context: dict[str, Any] | None,
) -> dict[str, Any]:
    payload = {} if trace_context is None else dict(trace_context)
    trace_id = clean_correlation_value("trace_id", payload.get("trace_id"))
    if trace_id is None:
        trace_id = clean_correlation_value("trace_id", payload.get("traceId"))
    if trace_id is None:
        payload["trace_id"] = get_invocation_id()
    return payload
