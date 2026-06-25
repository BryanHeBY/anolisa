"""Resolve Skill Ledger operations away from SkillFS runtime views."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from agent_sec_cli.skill_ledger.config import resolve_managed_skill_dirs
from agent_sec_cli.skill_ledger.core.file_hasher import (
    compute_file_hashes,
    diff_file_hashes,
)
from agent_sec_cli.skill_ledger.core.manifest_helpers import (
    safe_load_latest_manifest,
    snapshot_matches_manifest,
)
from agent_sec_cli.skill_ledger.core.manifest_integrity import (
    verify_manifest_integrity,
)
from agent_sec_cli.skill_ledger.core.version_chain import snapshot_dir_path
from agent_sec_cli.skill_ledger.errors import SkillLedgerError
from agent_sec_cli.skill_ledger.models.manifest import SignedManifest
from agent_sec_cli.skill_ledger.signing.base import SigningBackend
from agent_sec_cli.skill_ledger.utils import validate_skill_dir

_MANAGED_RESOLUTION_REASONS = frozenset({"configured_input", "configured"})


@dataclass(frozen=True)
class LiveSkillDirResolution:
    """Result of resolving a user-visible skill path to its live source path."""

    input_dir: Path
    skill_dir: Path | None
    resolved: bool
    reason: str | None = None


def resolve_live_skill_dir(
    skill_dir: str | Path,
    backend: SigningBackend,
) -> LiveSkillDirResolution:
    """Resolve *skill_dir* to the live backing root used for ledger writes.

    SkillFS can expose ordinary files from an activation snapshot while
    exposing live ``.skill-meta``.  Once a signed manifest exists, ordinary
    files at the input path are therefore not enough evidence that the input is
    the live source.  We prefer configured Skill Ledger roots and use the input
    path itself only when its root files match the latest signed manifest.
    """
    input_dir = Path(skill_dir)
    validate_skill_dir(str(input_dir))
    configured_input = _configured_input_skill_dir(input_dir)
    if configured_input is not None:
        return LiveSkillDirResolution(
            input_dir=input_dir,
            skill_dir=configured_input,
            resolved=True,
            reason="configured_input",
        )

    input_manifest = safe_load_latest_manifest(input_dir)
    if input_manifest is None:
        return LiveSkillDirResolution(
            input_dir=input_dir,
            skill_dir=input_dir,
            resolved=True,
            reason="no_manifest",
        )
    if not _trusted_latest_manifest(input_dir, input_manifest, backend):
        return LiveSkillDirResolution(
            input_dir=input_dir,
            skill_dir=None,
            resolved=False,
            reason="untrusted_metadata",
        )

    candidates = _matching_configured_skill_dirs(input_dir, input_manifest, backend)
    if len(candidates) == 1:
        return LiveSkillDirResolution(
            input_dir=input_dir,
            skill_dir=candidates[0],
            resolved=True,
            reason="configured",
        )
    if len(candidates) > 1:
        names = ", ".join(str(path) for path in candidates)
        raise SkillLedgerError(f"ambiguous live skill roots for {input_dir}: {names}")

    if _root_matches_manifest(input_dir, input_manifest):
        return LiveSkillDirResolution(
            input_dir=input_dir,
            skill_dir=input_dir,
            resolved=True,
            reason="input_root_matches_latest",
        )

    return LiveSkillDirResolution(
        input_dir=input_dir,
        skill_dir=None,
        resolved=False,
        reason="unresolved_runtime_view",
    )


def _configured_input_skill_dir(input_dir: Path) -> Path | None:
    try:
        input_resolved = input_dir.resolve()
    except OSError:
        return None
    for candidate in resolve_managed_skill_dirs():
        if candidate.name != input_dir.name:
            continue
        try:
            if candidate.resolve() == input_resolved:
                return candidate
        except OSError:
            continue
    return None


def require_live_skill_dir(
    skill_dir: str | Path,
    backend: SigningBackend,
) -> Path:
    """Return the live source root or raise a user-facing error."""
    resolution = resolve_live_skill_dir(skill_dir, backend)
    if resolution.skill_dir is not None:
        return resolution.skill_dir
    raise SkillLedgerError(
        f"cannot resolve live skill root for {Path(skill_dir)}; the path may be "
        "a SkillFS runtime view. Run the command against a Skill Ledger managed "
        "source/backing skill path or ensure managedSkillDirs points to "
        "source/backing roots."
    )


def live_skill_dir_manageability(
    resolution: LiveSkillDirResolution,
) -> tuple[bool, str]:
    """Return whether *resolution* names a daemon-manageable live root."""
    if resolution.skill_dir is None:
        return (
            False,
            "skill root is not resolvable from managedSkillDirs",
        )
    if resolution.reason not in _MANAGED_RESOLUTION_REASONS:
        return (
            False,
            "skill root is not configured in managedSkillDirs as a source/backing root",
        )
    return ledger_update_access(resolution.skill_dir)


def ledger_update_access(skill_dir: str | Path) -> tuple[bool, str]:
    """Return whether current process can update this skill's ledger state."""
    root = Path(skill_dir)
    meta = root / ".skill-meta"
    writable_target = meta if meta.exists() else root
    if os.access(writable_target, os.W_OK):
        return True, "skill root is managed and ledger state is writable"
    if meta.exists():
        return False, f"ledger metadata is not writable: {meta}"
    return False, f"skill root is not writable for ledger bootstrap: {root}"


def _matching_configured_skill_dirs(
    input_dir: Path,
    input_manifest: SignedManifest,
    backend: SigningBackend,
) -> list[Path]:
    candidates: list[Path] = []
    seen: set[Path] = set()
    for candidate in resolve_managed_skill_dirs():
        if candidate.name != input_dir.name:
            continue
        try:
            resolved = candidate.resolve()
        except OSError:
            continue
        if resolved in seen:
            continue
        seen.add(resolved)
        manifest = safe_load_latest_manifest(candidate)
        if manifest is None:
            continue
        if not _same_manifest(manifest, input_manifest):
            continue
        if not _trusted_latest_manifest(candidate, manifest, backend):
            continue
        candidates.append(candidate)
    return candidates


def _same_manifest(left: SignedManifest, right: SignedManifest) -> bool:
    return left.versionId == right.versionId and left.manifestHash == right.manifestHash


def _trusted_latest_manifest(
    skill_dir: Path,
    manifest: SignedManifest,
    backend: SigningBackend,
) -> bool:
    valid, _ = verify_manifest_integrity(manifest, backend)
    if not valid:
        return False
    return snapshot_matches_manifest(
        snapshot_dir_path(skill_dir, manifest.versionId),
        manifest,
    )


def _root_matches_manifest(skill_dir: Path, manifest: SignedManifest) -> bool:
    try:
        root_hashes = compute_file_hashes(skill_dir)
    except ValueError:
        return False
    return bool(diff_file_hashes(manifest.fileHashes, root_hashes)["match"])
