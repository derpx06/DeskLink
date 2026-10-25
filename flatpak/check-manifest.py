#!/usr/bin/env python3
"""Checks the Flatpak manifest against the rest of the tree, offline.

  flatpak/check-manifest.py            development: placeholders are reported
  flatpak/check-manifest.py --release  a release: a placeholder is an error

Always checked: the tag names the Cargo.toml/meson version, and every
finish-arg is explained in SECURITY.md. Release only: the git source carries
the tag's full commit (TODO(owner): set it after tagging).
"""
import json, re, sys
from pathlib import Path

root = Path(__file__).resolve().parent.parent
manifest = json.loads((root / "derx06.desklink.com.json").read_text())
errors, todo = [], []

version = re.search(r'^version = "([^"]+)"', (root / "Cargo.toml").read_text(), re.M).group(1)
source = next(s for s in manifest["modules"][0]["sources"] if isinstance(s, dict) and s.get("type") == "git")
if source.get("tag") != f"v{version}":
    errors.append(f"tag {source.get('tag')} is not v{version} (Cargo.toml)")
if not re.fullmatch(r"[0-9a-f]{40}", source.get("commit", "")):
    todo.append("TODO(owner): add the full commit sha of the tag next to \"tag\" (Flathub requires both)")

security = (root / "SECURITY.md").read_text()
for arg in manifest["finish-args"]:
    if f"`{arg}`" not in security:
        errors.append(f"{arg} is not justified in SECURITY.md")

if "DESKLINK_RELAY_URL" in json.dumps(manifest["build-options"]):
    todo.append("TODO(owner): DESKLINK_RELAY_URL is set in the manifest; release builds take it from the CI variable")

for line in todo:
    print(("error: " if "--release" in sys.argv else "note: ") + line)
    if "--release" in sys.argv:
        errors.append(line)
for line in errors:
    print("error:", line)
sys.exit(1 if errors else 0)
