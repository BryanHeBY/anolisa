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


def _fake_qwen(path, *, version_output="0.19.9", extensions_help=None):
    if extensions_help is None:
        extensions_help = """qwen extensions <command>

Manage Qwen Code extensions.

Commands:
  qwen extensions install <source>          Installs an extension.
  qwen extensions update [<name>] [--all]   Updates extensions.
  qwen extensions enable [--scope] <name>   Enables an extension.
"""
    _write_executable(
        path,
        f"""#!/usr/bin/env python3
import json
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
if args == ["--version"]:
    print({version_output!r})
    raise SystemExit(0)
if args == ["extensions", "--help"]:
    print({extensions_help!r})
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
        json.dumps({{"type": "local", "source": str(source)}})
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
    raise SystemExit(f"unexpected qwen invocation: {{args}}")
""",
    )


def _fake_agent_sec_cli(
    path, *, version_output="agent-sec-cli 0.8.0", valid_schema=True
):
    mapping = (
        {
            "before_agent_run": "before-agent",
            "before_tool_call": "before-tool",
            "after_tool_call": "after-tool",
            "after_agent_run": "after-agent",
        }
        if valid_schema
        else {}
    )
    _write_executable(
        path,
        f"""#!/usr/bin/env python3
import json
import sys

args = sys.argv[1:]
if args == ["--version"]:
    print({version_output!r})
elif args == ["observability", "schema"]:
    print(json.dumps({{"discriminator": {{"mapping": {mapping!r}}}}}))
elif args in (["observability", "record", "--help"], ["scan-pii", "--help"]):
    pass
else:
    raise SystemExit(f"unexpected agent-sec-cli invocation: {{args}}")
""",
    )


def _run_deploy(
    tmp_path,
    extension_dir,
    *,
    node_version="v22.0.0",
    qwen_version="0.19.9",
    qwen_extensions_help=None,
    agent_sec_cli_version="agent-sec-cli 0.8.0",
    valid_observability_schema=True,
):
    fake_qwen = tmp_path / "qwen"
    fake_cli = tmp_path / "agent-sec-cli"
    fake_node = tmp_path / "node"
    qwen_home = tmp_path / "qwen-home"
    log_path = tmp_path / "qwen.log"
    if log_path.exists():
        log_path.unlink()
    _fake_qwen(
        fake_qwen,
        version_output=qwen_version,
        extensions_help=qwen_extensions_help,
    )
    _fake_agent_sec_cli(
        fake_cli,
        version_output=agent_sec_cli_version,
        valid_schema=valid_observability_schema,
    )
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


def test_deploy_rejects_unrecognized_qwen_version_output(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(tmp_path, source, qwen_version="not-qwen")

    assert result.returncode == 1
    assert calls == []
    assert not (qwen_home / "extensions").exists()
    assert "unexpected qwen --version output" in result.stderr


def test_deploy_rejects_qwen_without_required_extension_interface(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(
        tmp_path,
        source,
        qwen_extensions_help="unrelated extension command",
    )

    assert result.returncode == 1
    assert calls == []
    assert not (qwen_home / "extensions").exists()
    assert "does not match the required Qwen Code interface" in result.stderr


def test_deploy_rejects_unrecognized_agent_sec_cli_version_output(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(
        tmp_path,
        source,
        agent_sec_cli_version="0.8.0",
    )

    assert result.returncode == 1
    assert calls == []
    assert not (qwen_home / "extensions").exists()
    assert "unexpected agent-sec-cli --version output" in result.stderr


def test_deploy_rejects_incompatible_agent_sec_cli_observability_schema(tmp_path):
    source = tmp_path / "extension-source"
    shutil.copytree(_EXTENSION_DIR, source)

    result, calls, qwen_home = _run_deploy(
        tmp_path,
        source,
        valid_observability_schema=False,
    )

    assert result.returncode == 1
    assert calls == []
    assert not (qwen_home / "extensions").exists()
    assert "observability schema is incompatible" in result.stderr
