"""Runtime activation resolver for daemon-stage ledger decisions.

The resolver keeps runtime policy decisions on the ledger side and writes a
minimal activation contract:

    {"schemaVersion": 1, "target": ".skill-meta/versions/v000001.snapshot"}

The daemon stage will reuse this internal helper when publishing the current
runtime target. A null target means no trusted snapshot is currently available.
"""

import json
import os
import tempfile
from pathlib import Path
from typing import Any

from agent_sec_cli.skill_ledger.core.checker import check
from agent_sec_cli.skill_ledger.core.file_hasher import (
    compute_snapshot_file_hashes,
    diff_file_hashes,
)
from agent_sec_cli.skill_ledger.core.manifest_integrity import (
    verify_manifest_integrity,
)
from agent_sec_cli.skill_ledger.core.version_chain import (
    SKILL_META_DIR,
    VERSIONS_DIR,
    ensure_skill_meta,
    list_version_ids,
    load_version_manifest,
    snapshot_dir_path,
)
from agent_sec_cli.skill_ledger.models.manifest import SignedManifest
from agent_sec_cli.skill_ledger.signing.base import SigningBackend
from agent_sec_cli.skill_ledger.utils import validate_skill_dir

SCHEMA_VERSION = 1
ACTIVATION_JSON = "activation.json"
ACTIVATION_XATTR = "user.agent_sec.skill_ledger.activation"
DEFAULT_ACTIVATION_POLICY = "pass_only"


def activation_json_path(skill_dir: str | Path) -> Path:
    """Return the activation contract path for *skill_dir*."""
    return Path(skill_dir) / SKILL_META_DIR / ACTIVATION_JSON


def activation_xattr_name() -> str:
    """Return the xattr name used for the activation contract."""
    return ACTIVATION_XATTR


def snapshot_target(version_id: str) -> str:
    """Return the SkillFS target path for a version snapshot."""
    return f"{SKILL_META_DIR}/{VERSIONS_DIR}/{version_id}.snapshot"


def resolve_activation(
    skill_dir: str,
    backend: SigningBackend,
    *,
    policy: str = DEFAULT_ACTIVATION_POLICY,
    write_activation: bool = True,
) -> dict[str, Any]:
    """Resolve and optionally persist the runtime activation target.

    The default ``pass_only`` policy activates only signed ``scanStatus=pass``
    versions with intact snapshots. Current source workspace changes never
    become runtime-readable until scan/certify creates a passing snapshot.
    """
    if policy != DEFAULT_ACTIVATION_POLICY:
        raise ValueError(f"unsupported activation policy: {policy}")

    validate_skill_dir(skill_dir)
    skill_name = Path(skill_dir).name
    status_result = check(skill_dir, backend)
    status = status_result.get("status", "unknown")

    candidate = find_latest_pass_snapshot(skill_dir, backend)
    if candidate is None:
        target = None
        active_version = None
    else:
        active_version, target = candidate

    activation = {"schemaVersion": SCHEMA_VERSION, "target": target}
    activation_xattr = _activation_xattr_status(written=False, skipped=True)
    if write_activation:
        activation_xattr = write_activation_contract(skill_dir, activation)

    return {
        "schemaVersion": SCHEMA_VERSION,
        "skillName": skill_name,
        "target": target,
        "activeVersionId": active_version,
        "status": status,
        "policy": policy,
        "activationPath": str(activation_json_path(skill_dir)),
        "activationXattr": activation_xattr,
    }


def find_latest_pass_snapshot(
    skill_dir: str | Path,
    backend: SigningBackend,
) -> tuple[str, str] | None:
    """Return ``(version_id, target)`` for the newest valid pass snapshot."""
    for version_id in reversed(list_version_ids(skill_dir)):
        try:
            manifest = load_version_manifest(skill_dir, version_id)
        except (json.JSONDecodeError, ValueError):
            continue
        if manifest is None:
            continue
        if manifest.versionId != version_id:
            continue
        if not _is_signed_pass_manifest(manifest, backend):
            continue
        if not _snapshot_matches_manifest(skill_dir, version_id, manifest):
            continue
        return version_id, snapshot_target(version_id)
    return None


def write_activation_contract(
    skill_dir: str | Path, activation: dict[str, Any]
) -> dict[str, Any]:
    """Atomically write the activation contract and best-effort xattr."""
    contract = _minimal_activation_contract(activation)
    meta_dir = ensure_skill_meta(skill_dir)
    path = meta_dir / ACTIVATION_JSON
    _atomic_write_json(path, contract)
    return write_activation_xattr(skill_dir, contract)


def write_activation_xattr(
    skill_dir: str | Path, activation: dict[str, Any]
) -> dict[str, Any]:
    """Best-effort write of the activation contract to the skill directory xattr."""
    setxattr = getattr(os, "setxattr", None)
    if setxattr is None:
        return _activation_xattr_status(
            written=False,
            available=False,
            error="os.setxattr unavailable",
        )

    contract = _minimal_activation_contract(activation)
    payload = json.dumps(
        contract,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    try:
        setxattr(str(Path(skill_dir)), ACTIVATION_XATTR, payload)
    except OSError as exc:
        return _activation_xattr_status(
            written=False,
            available=True,
            error=f"{type(exc).__name__}: {exc}",
        )
    return _activation_xattr_status(written=True, available=True)


def _is_signed_pass_manifest(
    manifest: SignedManifest,
    backend: SigningBackend,
) -> bool:
    if manifest.scanStatus != "pass":
        return False
    valid, _ = verify_manifest_integrity(manifest, backend)
    return valid


def _snapshot_matches_manifest(
    skill_dir: str | Path,
    version_id: str,
    manifest: SignedManifest,
) -> bool:
    snapshot_path = snapshot_dir_path(skill_dir, version_id)
    if not snapshot_path.is_dir():
        return False
    try:
        current_hashes = compute_snapshot_file_hashes(snapshot_path)
    except ValueError:
        return False
    return bool(diff_file_hashes(manifest.fileHashes, current_hashes)["match"])


def _minimal_activation_contract(activation: dict[str, Any]) -> dict[str, Any]:
    return {"schemaVersion": SCHEMA_VERSION, "target": activation.get("target")}


def _activation_xattr_status(
    *,
    written: bool,
    available: bool | None = None,
    error: str | None = None,
    skipped: bool = False,
) -> dict[str, Any]:
    status: dict[str, Any] = {
        "name": ACTIVATION_XATTR,
        "written": written,
    }
    if available is not None:
        status["available"] = available
    if error is not None:
        status["error"] = error
    if skipped:
        status["skipped"] = True
    return status


def _atomic_write_json(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with open(fd, "w", encoding="utf-8") as fh:
            json.dump(data, fh, ensure_ascii=False, indent=2)
            fh.write("\n")
            fh.flush()
        os.replace(tmp_path, path)
    except BaseException:
        Path(tmp_path).unlink(missing_ok=True)
        raise
