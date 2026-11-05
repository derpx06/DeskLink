#!/usr/bin/env python3
"""Writes src/icons/desklink-<name>-symbolic.svg for the Material Symbols
Rounded glyphs DeskLink uses (weight 400, grade 0, 24 px), and the
gresource entries for them (printed, to paste between the markers in
src/desklink.gresource.xml; --write does it).

Sources, in order: the phone's sym_* drawables (so both apps draw the same
glyph), then the Android SDK's icon set for names the phone lacks. Both are
Apache-2.0 (Google); see THIRD_PARTY_LICENSES.

usage: import-material-symbols.py PHONE_DRAWABLE_DIR SDK_SYMBOLS_ROUNDED_DIR [--write]
"""
import os, re, sys

phone, sdk = sys.argv[1], sys.argv[2]
here = os.path.dirname(os.path.abspath(__file__))
icons = os.path.join(here, "..", "src", "icons")

# Names the phone has not drawn yet (Material Symbols names).
EXTRA = [
    "check", "content_copy", "directory_sync", "document_scanner", "error",
    "history", "more_horiz", "photo_camera", "reply", "ring_volume", "schedule",
    "snooze", "wifi",
]

def path_data(xml):
    """(path, viewBox). Some drawables draw in the font's own space, y from
    -960 to 0, and move it down with <group android:translateY="960">: the
    same shift is the viewBox's origin."""
    m = re.search(r'android:pathData="([^"]+)"', xml)
    vw = re.search(r'android:viewportWidth="([\d.]+)"', xml).group(1)
    shift = re.search(r'android:translateY="([\d.]+)"', xml)
    top = f"-{shift.group(1)}" if shift else "0"
    return m.group(1), f"0 {top} {vw} {vw}"

found = {}
for f in sorted(os.listdir(phone)):
    if f.startswith("sym_") and f.endswith(".xml"):
        found[f[4:-4]] = path_data(open(os.path.join(phone, f)).read())
for name in EXTRA:
    if name not in found:
        found[name] = path_data(open(os.path.join(sdk, name, f"rounded_{name}_24.xml")).read())

entries = []
for name, (d, view_box) in sorted(found.items()):
    stem = "desklink-" + name.replace("_", "-") + "-symbolic"
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" '
           f'viewBox="{view_box}"><path fill="#222" d="{d}"/></svg>\n')
    open(os.path.join(icons, stem + ".svg"), "w").write(svg)
    entries.append(f'    <file alias="icons/scalable/actions/{stem}.svg">icons/{stem}.svg</file>')

block = "\n".join(entries)
if "--write" in sys.argv:
    xml_path = os.path.join(here, "..", "src", "desklink.gresource.xml")
    xml = open(xml_path).read()
    begin, end = "    <!-- material-symbols:begin -->\n", "    <!-- material-symbols:end -->\n"
    if begin in xml:
        xml = xml[: xml.index(begin)] + begin + block + "\n" + xml[xml.index(end):]
    else:
        xml = xml.replace("  </gresource>", begin + block + "\n" + end + "  </gresource>")
    open(xml_path, "w").write(xml)
print(len(entries), "icons")
