# SPDX-License-Identifier: GPL-2.0-or-later
UUID = desklink@derx06.github.io

pack:
	gnome-extensions pack --force $(UUID)

install: pack
	gnome-extensions install --force $(UUID).shell-extension.zip

.PHONY: pack install
