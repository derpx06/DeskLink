// SPDX-License-Identifier: GPL-2.0-or-later
//
// DeskLink in the GNOME top bar: a pill with the link glyph (computer, wire,
// phone) and a coral dot while a phone is using this computer. Its menu
// lists what is live now, each with End, then End Everything, the desk's
// tiles (Keep Awake, Clipboard, Send Files, Ring Phone), Lock & Hand Off and
// Open DeskLink.
//
// On the lock screen it shows only "Unlock with <phone>", and only while the
// app offers that (the user turned it on in DeskLink and the phone is on
// this network); the phone still has to approve.
//
// Everything comes from the running app over D-Bus
// (derx06.desklink.com.Shell1 at /derx06/desklink/com/Shell); nothing is shown
// while DeskLink is not running.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const BUS_NAME = 'derx06.desklink.com';
const OBJECT_PATH = '/derx06/desklink/com/Shell';
const DESKTOP_FILE = 'derx06.desklink.com.desktop';

const ShellProxy = Gio.DBusProxy.makeProxyWrapper(`<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
  </interface>
</node>`);

const PATHS = {
    lan: 'Local network',
    direct: 'Over the internet',
    turn: 'Through a relay server',
};

function title(device) {
    return device.battery >= 0 ? `${device.name} · ${device.battery}%` : device.name;
}

function pathLine(device) {
    if (!device.reachable)
        return 'Not connected';
    return PATHS[device.path] ?? (device.ready ? '' : 'Connecting…');
}

function sessionLine(device) {
    const parts = [];
    if (device.screen === 'control')
        parts.push('controlling the screen');
    else if (device.screen === 'view')
        parts.push('viewing the screen');
    if (device.terminals > 0)
        parts.push(device.terminals === 1 ? '1 terminal' : `${device.terminals} terminals`);
    return parts.join(' · ');
}

function centered(params) {
    return {y_align: Clutter.ActorAlign.CENTER, ...params};
}

const DeskLinkButton = GObject.registerClass(
class DeskLinkButton extends PanelMenu.Button {
    _init() {
        super._init(0.5, 'DeskLink');
        const pill = new St.BoxLayout(centered({style_class: 'desklink-pill'}));
        this._computer = new St.Widget(centered({style_class: 'desklink-glyph-computer'}));
        this._wire = new St.Widget(centered({style_class: 'desklink-glyph-wire'}));
        this._phone = new St.Widget(centered({style_class: 'desklink-glyph-phone'}));
        this._dot = new St.Widget(centered({style_class: 'desklink-live-dot', visible: false}));
        this._label = new St.Label(centered({style_class: 'desklink-pill-label', visible: false}));
        for (const part of [this._computer, this._wire, this._phone, this._dot, this._label])
            pill.add_child(part);
        this.add_child(pill);
    }

    /// `online`: a phone is connected; `live`: one is using this computer.
    showState(online, live, words) {
        for (const part of [this._wire, this._phone]) {
            if (online)
                part.add_style_class_name('live');
            else
                part.remove_style_class_name('live');
        }
        this._dot.visible = live;
        this.accessible_name = `DeskLink: ${words}`;
    }

    showLabel(text) {
        this._label.text = text ?? '';
        this._label.visible = !!text;
    }
});

export default class DeskLinkExtension extends Extension {
    // "unlock-dialog" is in session-modes for one item: on the lock screen
    // the menu offers "Unlock with <phone>" (DeskLink asks the phone, which
    // must approve). Nothing else is shown or invokable while locked.
    enable() {
        this._button = new DeskLinkButton();
        Main.panel.addToStatusArea(this.uuid, this._button);
        this._modeId = Main.sessionMode.connect('updated', () => this._sync());
        this._sync();

        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => this._disconnect());
    }

    disable() {
        Gio.bus_unwatch_name(this._watchId);
        this._watchId = 0;
        Main.sessionMode.disconnect(this._modeId);
        this._modeId = 0;
        this._disconnect();
        this._button.destroy();
        this._button = null;
    }

    _connect() {
        this._disconnect();
        const cancellable = new Gio.Cancellable();
        this._cancellable = cancellable;
        // `new` works whether the wrapper is a function (GNOME ≤ 50) or a
        // Gio.DBusProxy subclass (GNOME 51).
        new ShellProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, (proxy, error) => {
            if (cancellable.is_cancelled())
                return;
            if (error) {
                console.warn(`DeskLink: ${error.message}`);
                return;
            }
            this._proxy = proxy;
            this._changedId = proxy.connect('g-properties-changed', () => this._sync());
            this._sync();
        }, cancellable, Gio.DBusProxyFlags.DO_NOT_AUTO_START);
    }

    _disconnect() {
        this._cancellable?.cancel();
        this._cancellable = null;
        if (this._changedId)
            this._proxy.disconnect(this._changedId);
        this._changedId = 0;
        this._proxy = null;
        if (this._button)
            this._sync();
    }

    _sync() {
        const locked = Main.sessionMode.isLocked;
        const running = !!this._proxy;
        const devices = (this._proxy?.get_cached_property('Devices')?.recursiveUnpack() ?? [])
            .filter(device => device.paired);
        const desk = this._proxy?.get_cached_property('Desk')?.recursiveUnpack() ?? {};
        const ready = devices.find(device => device.ready);
        const shown = ready ?? devices.find(device => device.reachable);
        const live = devices.filter(device => device.screen || device.terminals > 0);
        const unlockers = devices.filter(device => device.actions.includes('unlock-via-phone'));
        const menu = this._button.menu;
        menu.removeAll();

        if (locked) {
            this._button.visible = running && unlockers.length > 0;
            this._button.showState(true, false, 'unlock with your phone');
            this._button.showLabel(unlockers.length === 1 ? `Unlock with ${unlockers[0].name}` : 'Unlock');
            for (const device of unlockers)
                menu.addAction(`Unlock with ${device.name}`, () => this._invoke(device.id, 'unlock-via-phone'));
            return;
        }

        this._button.visible = running;
        this._button.showLabel(null);
        this._button.showState(!!ready, live.length > 0,
            shown ? `${title(shown)}, ${pathLine(shown)}` : 'no phone connected');

        const head = new PopupMenu.PopupMenuItem(shown ? title(shown) : 'No phone connected', {reactive: false});
        head.label.add_style_class_name('desklink-device-name');
        menu.addMenuItem(head);
        if (shown && pathLine(shown))
            menu.addMenuItem(new PopupMenu.PopupMenuItem(pathLine(shown), {reactive: false}));

        if (live.length > 0) {
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Live now'));
            for (const device of live)
                menu.addMenuItem(this._liveRow(device));
            menu.addAction('End Everything', () => this._invoke('', 'end-sessions'));
        }

        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        menu.addMenuItem(this._tiles(desk, ready));
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const handOff = menu.addAction('Lock & Hand Off', () => this._invoke('', 'lock-hand-off'));
        handOff.setSensitive(!!desk.handOff);
        menu.addAction('Open DeskLink', () => this._invoke('', 'open'));
    }

    /// "Pixel · viewing the screen · 2 terminals" with an End button.
    _liveRow(device) {
        const row = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        row.add_child(new St.Label(centered({
            text: `${device.name} · ${sessionLine(device)}`,
            x_expand: true,
        })));
        const end = new St.Button({
            label: 'End',
            style_class: 'desklink-end-button',
            can_focus: true,
            accessible_name: `End what ${device.name} is doing here`,
        });
        end.connect('clicked', () => this._invoke(device.id, 'end-sessions'));
        row.add_child(end);
        return row;
    }

    /// Keep Awake and Clipboard (switches), Send Files and Ring Phone.
    _tiles(desk, ready) {
        const row = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const box = new St.BoxLayout({style_class: 'desklink-tiles', x_expand: true});
        const tile = (icon, label, on, enabled, action) => {
            const content = new St.BoxLayout({vertical: true, x_align: Clutter.ActorAlign.CENTER});
            content.add_child(new St.Icon({icon_name: icon, style_class: 'popup-menu-icon'}));
            content.add_child(new St.Label({text: label, style_class: 'desklink-tile-label'}));
            const button = new St.Button({
                child: content,
                style_class: 'desklink-tile',
                can_focus: enabled,
                reactive: enabled,
                x_expand: true,
                accessible_name: on === null ? label : `${label}, ${on ? 'on' : 'off'}`,
            });
            if (on)
                button.add_style_pseudo_class('checked');
            if (!enabled)
                button.add_style_pseudo_class('insensitive');
            button.connect('clicked', action);
            box.add_child(button);
        };
        const switches = desk.keepAwake !== undefined;
        if (switches) {
            tile('weather-clear-night-symbolic', 'Keep Awake', desk.keepAwake, true,
                () => this._invoke('', desk.keepAwake ? 'keep-awake-off' : 'keep-awake-on'));
            tile('edit-paste-symbolic', 'Clipboard', desk.clipboard, true,
                () => this._invoke('', desk.clipboard ? 'clipboard-off' : 'clipboard-on'));
        }
        const can = action => !!ready && ready.actions.includes(action);
        tile('document-send-symbolic', 'Send Files', null, can('send-files'),
            () => this._invoke(ready.id, 'send-files'));
        tile('audio-volume-high-symbolic', 'Ring Phone', null, can('ring'),
            () => this._invoke(ready.id, 'ring'));
        row.add_child(box);
        return row;
    }

    _invoke(deviceId, action) {
        this._button?.menu.close();
        if (!this._proxy) {
            if (Main.sessionMode.isLocked)
                return;
            Shell.AppSystem.get_default().lookup_app(DESKTOP_FILE)?.activate();
            return;
        }
        this._proxy.InvokeRemote(deviceId, action, (_result, error) => {
            if (error)
                console.warn(`DeskLink: ${action} failed: ${error.message}`);
        });
    }
}
