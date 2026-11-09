#!/usr/bin/env python3
"""Offline packaging checks; no device access or service commands."""

import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
manifest = json.loads((ROOT / "plugin.json").read_text())
metadata = subprocess.run(
    ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
    cwd=ROOT, check=True, capture_output=True, text=True,
)
package = json.loads(metadata.stdout)["packages"][0]
assert manifest == {
    "api_version": 1,
    "id": "corsair",
    "name": "Corsair keyboards",
    "description": manifest["description"],
    "version": package["version"],
    "executable": package["name"],
    "update_repo": "mikkeyboi/hyprdeck-corsair",
    "asset": "hyprdeck-corsair-linux-x86_64",
}, "Cargo/plugin/release identity mismatch"

supported = {"2b07", "2b10", "2b11", "2b23", "1b7c", "1b7d", "1bc5"}
hid = (ROOT / "data/70-hyprdeck-corsair.rules").read_text().splitlines()
hid = [line for line in hid if line and not line.startswith("#")]
assert len(hid) == 15
assert 'IMPORT{builtin}="usb_id"' in hid[0]
assert set(re.search(r'ATTRS\{idProduct\}=="([^"]+)"', hid[0])[1].split("|")) == supported
pairs = set()
for line in hid[1:]:
    assert 'SUBSYSTEM=="hidraw"' in line and 'ATTRS{idVendor}=="1b1c"' in line
    assert 'ACTION=="add|change"' in line and 'TAG+="uaccess"' in line
    pairs.add((re.search(r'ATTRS\{idProduct\}=="([^"]+)"', line)[1],
               re.search(r'ENV\{ID_USB_INTERFACE_NUM\}=="([^"]+)"', line)[1]))
assert pairs == {(pid, interface) for pid in supported for interface in ("01", "02")}

inputs = (ROOT / "data/71-hyprdeck-corsair-input.rules").read_text().splitlines()
inputs = [line for line in inputs if line and not line.startswith("#")]
assert len(inputs) == 9
assert inputs[0] == 'KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess", OPTIONS+="static_node=uinput"'
assert {re.search(r'ATTRS\{idProduct\}=="([^"]+)"', line)[1] for line in inputs[1:-1]} == supported
for line in inputs[1:]:
    assert 'ACTION=="add|change"' in line and 'SUBSYSTEM=="input"' in line
    assert 'KERNEL=="event*"' in line and 'TAG+="uaccess"' in line
assert 'ATTRS{name}=="hyprdeck-corsair*"' in inputs[-1]
for line in hid + inputs:
    assert not any(token in line for token in ('MODE=', 'GROUP=', 'RUN'))
    assert line.count('"') % 2 == 0
for path in (ROOT / "README.md", ROOT / "docs/DEVELOPMENT.md"):
    text = path.read_text()
    for target in re.findall(r'\]\(([^)]+)\)', text):
        if "://" not in target and not target.startswith("#"):
            assert (path.parent / target.split("#")[0]).exists(), f"Broken link: {target}"
print("Packaging identity, 14 HID grants, 9 optional input rules and documentation links: OK")
