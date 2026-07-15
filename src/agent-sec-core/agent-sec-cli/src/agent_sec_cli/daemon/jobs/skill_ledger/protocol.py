"""Data models shared by Skill Ledger activation scheduling and processing."""

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


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
