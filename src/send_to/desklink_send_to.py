# desklink-send-to-extension 1
# Written by DeskLink; removed when "Show Send to phone in the file manager"
# is switched off or the last phone is unpaired. Do not edit: it is rewritten.
#
# Runs INSIDE Files (Nautilus), so it is tiny and defensive: no D-Bus, no
# network, no file contents. The menu reads one small JSON file DeskLink keeps
# up to date (the DeskLink program and the paired phones), and a click starts
# `desklink --send-files` detached, with the paths as separate arguments (never
# through a shell).
import gettext
import json
import os
import subprocess
import threading

import gi

for _version in ("4.1", "4.0"):
    try:
        gi.require_version("Nautilus", _version)
        break
    except Exception:
        pass
from gi.repository import GObject, Nautilus  # noqa: E402

_ = gettext.translation("desklink", fallback=True).gettext

PHONES_FILE = os.path.join(
    os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share"),
    "desklink",
    "send-to-phones.json",
)
MAX_ENTRIES = 500  # DeskLink refuses more, out loud; one past it says so


def read_phones(path=PHONES_FILE):
    """(program, [(id, name), ...]) from DeskLink's file; (None, []) if unusable."""
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.loads(handle.read(65536))
        program = data["exe"]
        if not isinstance(program, str) or not os.path.isabs(program):
            return None, []
        phones = []
        for phone in data["phones"]:
            if isinstance(phone["id"], str) and isinstance(phone["name"], str):
                phones.append((phone["id"], phone["name"]))
        return program, phones
    except Exception:
        return None, []


def local_paths(files):
    """(paths of the local items, whether one of them is not a folder)."""
    paths, has_file = [], False
    for item in files:
        try:
            if item.get_uri_scheme() != "file":
                continue
            path = item.get_location().get_path()
            if not path:
                continue
            paths.append(path)
            if not item.is_directory():
                has_file = True
        except Exception:
            continue
    return paths, has_file


def send(program, phone_id, paths):
    """Starts DeskLink's command, detached; never waits, never raises."""
    try:
        process = subprocess.Popen(
            [program, "--send-files", "--to", phone_id, "--"] + list(paths[: MAX_ENTRIES + 1]),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
        threading.Thread(target=process.wait, daemon=True).start()
    except Exception:
        pass


def _item(name, label, tip):
    try:
        return Nautilus.MenuItem(
            name=name, label=label, tip=tip, icon="derx06.desklink.com-symbolic"
        )
    except Exception:
        return Nautilus.MenuItem(name=name, label=label, tip=tip)


def build_items(program, phones, paths):
    """One "Send to phone" entry for one phone, a submenu for several."""
    tip = _("Send the selected files to your phone with DeskLink")
    if len(phones) == 1:
        item = _item("DeskLink::SendTo", _("Send to phone"), tip)
        item.connect("activate", lambda _i, p=phones[0][0]: send(program, p, paths))
        return [item]
    item = _item("DeskLink::SendTo", _("Send to phone"), tip)
    menu = Nautilus.Menu()
    for index, (phone_id, name) in enumerate(phones):
        entry = _item("DeskLink::SendTo::%d" % index, _("Send to %s") % name, tip)
        entry.connect("activate", lambda _i, p=phone_id: send(program, p, paths))
        menu.append_item(entry)
    item.set_submenu(menu)
    return [item]


class SendToPhone(GObject.GObject, Nautilus.MenuProvider):
    def get_file_items(self, *args):
        # Nautilus 4: (files); older: (window, files).
        try:
            program, phones = read_phones()
            if not program or not phones:
                return []
            paths, has_file = local_paths(args[-1])
            if not has_file:
                return []
            return build_items(program, phones, paths)
        except Exception:
            return []

    def get_background_items(self, *args):
        return []
