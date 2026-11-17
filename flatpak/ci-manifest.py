#!/usr/bin/env python3
"""Writes ci-manifest.json: the Flatpak manifest with the desklink module built
from this checkout (a `dir` source in place of its git tag) and, when the CI
variable is set, the relay address. Run from the repository root; the release
workflow does, and so can you to see what it will build:

  python3 flatpak/ci-manifest.py && python3 -c "import json;print(json.load(open('ci-manifest.json'))['modules'])"

The module is chosen by name: the manifest lists vte first, and replacing the
first module's source broke the release build.
"""
import json, os, sys

manifest = json.load(open("derx06.desklink.com.json"))
relay = os.environ.get("DESKLINK_RELAY_URL", "")
if relay:
    manifest["build-options"]["env"]["DESKLINK_RELAY_URL"] = relay

modules = [m for m in manifest["modules"] if isinstance(m, dict) and m.get("name") == "desklink"]
if len(modules) != 1:
    sys.exit("error: the manifest must have exactly one module named desklink")
sources = modules[0]["sources"]
git = [i for i, s in enumerate(sources) if isinstance(s, dict) and s.get("type") == "git"]
if len(git) != 1:
    sys.exit("error: the desklink module must have exactly one git source")
sources[git[0]] = {
    "type": "dir",
    "path": ".",
    "skip": [".flatpak-builder", "build-flatpak", "repo", "target"],
}
json.dump(manifest, open("ci-manifest.json", "w"), indent=2)
