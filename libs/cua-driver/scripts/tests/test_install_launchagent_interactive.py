"""The local LaunchAgent runs as ProcessType Interactive.

launchd spawns an agent without ProcessType as a background daemon and
throttles its timers, so every sleep in cua-driver-local ran 2 to 3 times
long (an 8 ms key gap took 27 to 45 ms) and long type_text calls outlived
the client's 120 s deadline. Reinstalls must add the key to an existing
agent without touching its arguments or environment.
"""

from __future__ import annotations

import plistlib
import re
import shutil
import subprocess
from pathlib import Path

import pytest

INSTALL_LOCAL = Path(__file__).resolve().parents[1] / "_install-local-rust.sh"
AGENT = "$HOME/Library/LaunchAgents/com.trycua.cua-driver-local.plist"
requires_plutil = pytest.mark.skipif(shutil.which("plutil") is None, reason="requires plutil")

# An agent written by an older installer and then customized by hand.
CUSTOMIZED_AGENT = {
    "EnvironmentVariables": {"CUA_LOG": "info"},
    "KeepAlive": True,
    "Label": "com.trycua.cua-driver-local",
    "ProgramArguments": [
        "/Users/someone/.local/bin/cua-driver-local",
        "serve",
        "--grant",
        "existing-profile",
        "--experimental-pip",
    ],
    "RunAtLoad": True,
    "StandardErrorPath": "/Users/someone/.cua-driver-local/serve.err.log",
    "StandardOutPath": "/Users/someone/.cua-driver-local/serve.out.log",
}


def _source() -> str:
    return INSTALL_LOCAL.read_text(encoding="utf-8")


def _function(name: str) -> str:
    match = re.search(rf"^{name}\(\) \{{\n.*?^\}}\n", _source(), re.S | re.M)
    assert match, f"{name} not found in {INSTALL_LOCAL.name}"
    return match.group(0)


def _make_interactive(plist: Path) -> None:
    script = _function("make_launchagent_interactive") + f'make_launchagent_interactive "{plist}"\n'
    subprocess.run(["/bin/bash", "-c", script], check=True)


@requires_plutil
def test_an_existing_agent_gains_interactive_and_keeps_everything_else(tmp_path: Path) -> None:
    plist = tmp_path / "agent.plist"
    plist.write_bytes(plistlib.dumps(CUSTOMIZED_AGENT))

    _make_interactive(plist)
    _make_interactive(plist)  # idempotent across reinstalls

    assert plistlib.loads(plist.read_bytes()) == {**CUSTOMIZED_AGENT, "ProcessType": "Interactive"}


@requires_plutil
def test_a_missing_agent_is_not_created(tmp_path: Path) -> None:
    plist = tmp_path / "agent.plist"
    _make_interactive(plist)
    assert not plist.exists()


def test_every_macos_install_updates_the_agent_it_unloads() -> None:
    source = _source()
    unload = source.index(f'launchctl unload "{AGENT}"')
    assert f'make_launchagent_interactive "{AGENT}"' in source[unload : unload + 300]


def test_a_new_agent_is_written_interactive() -> None:
    source = _source()
    autostart = source[source.index(f'PLIST_PATH="{AGENT}"') :]
    heredoc = autostart[: autostart.index("\nEOF\n")]
    assert "<key>ProcessType</key><string>Interactive</string>" in heredoc
