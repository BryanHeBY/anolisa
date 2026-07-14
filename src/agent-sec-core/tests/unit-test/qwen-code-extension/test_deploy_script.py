"""Tests for the Qwen Code extension deployment script."""

import json
import os
import shutil
import subprocess
from pathlib import Path

_ROOT = Path(__file__).resolve().parents[3]
_EXTENSION_DIR = _ROOT / "qwen-code-extension"
_DEPLOY_SCRIPT = _EXTENSION_DIR / "scripts" / "deploy.sh"


def _write_executable(path, content):
    path.write_text(content)
    path.chmod(0o755)


def _fake_qwen(path):
    _write_executable(
        path,
        """#!/usr/bin/env python3
import json
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
if args == ["--version"]:
    print("0.19.9")
    raise SystemExit(0)

log_path = Path(os.environ["QWEN_TEST_LOG"])
with log_path.open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(args) + "\\n")

home = Path(os.environ["QWEN_HOME"])
if args[:2] == ["extensions", "install"]:
    source = Path(args[2]).resolve()
    manifest = json.loads((source / "qwen-extension.json").read_text())
    target = home / "extensions" / manifest["name"]
    shutil.copytree(source, target)
    (target / ".qwen-extension-install.json").write_text(
        json.dumps({"type": "local", "source": str(source)})
    )
elif args[:2] == ["extensions", "update"]:
    target = home / "extensions" / args[2]
    metadata = json.loads((target / ".qwen-extension-install.json").read_text())
    source = Path(metadata["source"])
    shutil.rmtree(target)
    shutil.copytree(source, target)
    (target / ".qwen-extension-install.json").write_text(json.dumps(metadata))
elif args[:2] == ["extensions", "enable"]:
    pass
else:
    raise SystemExit(f"unexpected qwen invocation: {args}")
""",
    )


def _run_deploy(tmp_path, extension_dir, *, node_version="v22.0.0"):
    fake_qwen = tmp_path / "qwen"
    fake_cli = tmp_path / "agent-sec-cli"
    fake_node = tmp_path / "node"
    qwen_home = tmp_path / "qwen-home"
    log_path = tmp_path / "qwen.log"
    if log_path.exists():
        log_path.unlink()
    _fake_qwen(fake_qwen)
    _write_executable(fake_cli, "#!/usr/bin/env bash\nexit 0\n")
    _write_executable(fake_node, f"#!/usr/bin/env bash\necho {node_version}\n")

    env = os.environ.copy()
    env.update(
        {
            "QWEN_BIN": str(fake_qwen),
            "QWEN_HOME": str(qwen_home),
            "QWEN_TEST_LOG": str(log_path),
            "PATH": f"{tmp_path}:{env['PATH']}",
        }
    )
    result = subprocess.run(
        [str(_DEPLOY_SCRIPT), str(extension_dir)],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    calls = (
        [json.loads(line) for line in log_path.read_text().splitlines()]
        if log_path.exists()
        else []
    )
    return result, calls, qwen_home


def test_fresh_deploy_installs_and_enables_user_scope(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(tmp_path, source)

    manifest = json.loads((source / "qwen-extension.json").read_text())
    target = qwen_home / "extensions" / manifest["name"]
    assert result.returncode == 0, result.stderr
    assert calls == [
        ["extensions", "install", str(source), "--consent", "--scope", "user"],
        ["extensions", "enable", "--scope", "user", manifest["name"]],
    ]
    assert json.loads((target / "qwen-extension.json").read_text())["version"] == (
        manifest["version"]
    )


def test_deploy_updates_when_manifest_version_changes(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)
    manifest = json.loads((source / "qwen-extension.json").read_text())
    qwen_home = tmp_path / "qwen-home"
    target = qwen_home / "extensions" / manifest["name"]
    shutil.copytree(source, target)
    installed_manifest = dict(manifest)
    installed_manifest["version"] = "0.7.0"
    (target / "qwen-extension.json").write_text(json.dumps(installed_manifest))
    (target / ".qwen-extension-install.json").write_text(
        json.dumps({"type": "local", "source": str(source)})
    )

    result, calls, _ = _run_deploy(tmp_path, source)

    assert result.returncode == 0, result.stderr
    assert calls == [
        ["extensions", "update", manifest["name"]],
        ["extensions", "enable", "--scope", "user", manifest["name"]],
    ]
    assert json.loads((target / "qwen-extension.json").read_text())["version"] == (
        manifest["version"]
    )


def test_deploy_is_idempotent_at_same_version(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    first, _, _ = _run_deploy(tmp_path, source)
    second, calls, _ = _run_deploy(tmp_path, source)

    assert first.returncode == 0
    assert second.returncode == 0, second.stderr
    assert calls == [
        [
            "extensions",
            "enable",
            "--scope",
            "user",
            "agent-sec-core-qwen-code-extension",
        ],
    ]
    assert "is already installed" in second.stdout


def test_deploy_rejects_unsupported_node_before_install(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(tmp_path, source, node_version="v18.19.1")

    assert result.returncode == 1
    assert calls == []
    assert not (qwen_home / "extensions").exists()
    assert "requires Node.js >=22; found v18.19.1" in result.stderr
