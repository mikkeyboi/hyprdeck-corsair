#!/usr/bin/env python3
"""Exercise the release CLI with empty device trees and isolated XDG state."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix="corsair-smoke-") as root:
    root = Path(root)
    for name in ("sys", "dev", "config", "data", "state", "cache", "run"):
        (root / name).mkdir(mode=0o700)
    runtime_fd = os.open(root / "run", os.O_RDONLY | os.O_DIRECTORY)
    try:
        env = dict(os.environ)
        env.update({
            "HYPRDECK_CORSAIR_SYSFS": str(root / "sys"),
            "HYPRDECK_CORSAIR_DEV": str(root / "dev"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_STATE_HOME": str(root / "state"),
            "XDG_CACHE_HOME": str(root / "cache"),
            # Short alias into the private scratchpad directory; no outside files or sockets.
            "XDG_RUNTIME_DIR": f"/proc/self/fd/{runtime_fd}",
        })

        def run(args, payload=None, code=0):
            result = subprocess.run(
                [str(binary), *args], input=payload, text=True,
                capture_output=True, env=env, pass_fds=(runtime_fd,), timeout=20,
            )
            assert result.returncode == code, (args, result.returncode, result.stderr)
            return result.stdout

        def request(payload, success):
            output = run(["--request"], payload)
            assert len(output.splitlines()) == 1 and len(output.encode()) < 1024 * 1024
            envelope = json.loads(output)
            assert envelope["api_version"] == 1
            if success:
                assert envelope["error"] is None
                assert envelope["state"]["products"] == []
                assert len(envelope["state"]["groups"]) == 1
                assert envelope["state"]["groups"][0]["id"] == "no-keyboards"
            else:
                assert isinstance(envelope["error"], str) and envelope["error"]
                assert envelope["state"] is None

        run(["--help"])
        manifest = json.loads((Path(__file__).resolve().parents[1] / "plugin.json").read_text())
        assert run(["--version"]).strip() == f"hyprdeck-corsair {manifest['version']}"
        state = {"api_version": 1, "method": "state", "action": None, "args": {}}
        request(json.dumps(state), True)
        request(json.dumps({**state, "host_features": ["keyboard_visualization"]}), True)
        request('{"api_version":2,"method":"state","args":{}}', False)
        request('{"api_version":1,"method":"action","action":"no-such-action","args":{}}', False)
        # The missing device is rejected before opening any transport.
        request('{"api_version":1,"method":"action","action":"set-brightness","args":{"device":"k65plus-missing","value":50}}', False)
        request('{', False)
        request('x' * (1024 * 1024 + 1), False)
        assert json.loads(run(["state"]))["error"] is None
        assert json.loads(run(["action", "no-such-action"], code=1))["state"] is None
        run(["no-such-command"], code=2)
        assert not any(path.is_file() or path.is_socket() for path in root.rglob("*"))
        assert not any((root / "sys").iterdir()) and not any((root / "dev").iterdir())
        print("12 isolated CLI/protocol cases passed; no files or sockets written")
    finally:
        os.close(runtime_fd)
