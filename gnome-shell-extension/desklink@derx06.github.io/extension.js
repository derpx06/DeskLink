// SPDX-License-Identifier: GPL-2.0-or-later
//
// DeskLink in GNOME Shell.
//
// Quick Settings is the primary surface: a toggle named after the phone
// ("Connected · 82% · charging", or "last seen 14:02" while its link is down)
// whose menu lists the phones, quick actions (Ring, Lock, Send Files, Send
// clipboard, Keep Awake), what is live on this computer (each with End, and
// End Everything), recent transfers, Lock & Hand Off and Open DeskLink. A
// small status icon shows in the top bar while a phone is connected.
//
// The phones' notifications become GNOME notifications (notifications.js).
//
// An extra pill in the top bar (the link glyph, a coral dot while a phone is
// using this computer) is optional (DeskLink's Preferences, off by default).
// On the lock screen it shows only "Unlock with <phone>", and only while the
// app offers that; the phone still has to approve.
//
// Everything comes from the running app over D-Bus
// (derx06.desklink.com.Shell1 at /derx06/desklink/com/Shell); nothing is shown
// while DeskLink is not running.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {PhoneNotifications} from './notifications.js';

const BUS_NAME = 'derx06.desklink.com';
const OBJECT_PATH = '/derx06/desklink/com/Shell';
const DESKTOP_FILE = 'derx06.desklink.com.desktop';
const PHONE_ICON = 'phone-symbolic';

const ShellProxy = Gio.DBusProxy.makeProxyWrapper(`<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <property name="Notifications" type="aa{sv}" access="read"/>
    <property name="Transfers" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationAction">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationReply">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="text" type="s" direction="in"/>
    </method>
    <method name="NotificationDismiss">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
    </method>
    <method name="OpenTransfer">
      <arg name="transfer_id" type="s" direction="in"/>
    </method>
  </interface>
</node>`);

function lastSeen(device) {
    if (!device.lastSeen)
        return '';
    const seen = GLib.DateTime.new_from_unix_local(device.lastSeen);
    const now = GLib.DateTime.new_now_local();
    const today = seen.get_ymd().join() === now.get_ymd().join();
    return `last seen ${seen.format(today ? '%H:%M' : '%-d %b %H:%M')}`;
}

/// "Connected · 82% · charging", "last seen 14:02", "Connecting…".
function statusLine(device) {
    if (!device)
        return 'No phone connected';
    if (device.ready) {
        const parts = ['Connected'];
        if (device.battery >= 0)
            parts.push(`${device.battery}%`);
        if (device.charging)
            parts.push('charging');
        return parts.join(' · ');
    }
    if (device.reachable)
        return 'Connecting…';
    return lastSeen(device) || 'Not connected';
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

/// A stock-styled button: icon over a label, accent when checked.
function actionButton(icon, label, {name = label, checked = null, enabled = true, onClick}) {
    const content = new St.BoxLayout({vertical: true, x_align: Clutter.ActorAlign.CENTER});
    content.add_child(new St.Icon({icon_name: icon, style_class: 'popup-menu-icon'}));
    content.add_child(new St.Label({text: label, style_class: 'desklink-action-label'}));
    const button = new St.Button({
        child: content,
        style_class: 'button desklink-action',
        can_focus: enabled,
        reactive: enabled,
        x_expand: true,
        toggle_mode: checked !== null,
        checked: !!checked,
        accessible_name: checked === null ? name : `${name}, ${checked ? 'on' : 'off'}`,
    });
    if (!enabled)
        button.add_style_pseudo_class('insensitive');
    button.connect('clicked', onClick);
    return button;
}

const DeskLinkToggle = GObject.registerClass(
class DeskLinkToggle extends QuickMenuToggle {
    _init() {
        super._init({iconName: PHONE_ICON, toggleMode: false});
        this._setTitle('DeskLink', '');
    }

    /// `title`/`subtitle` since GNOME 44; set each only if this Shell has it.
    _setTitle(title, subtitle) {
        if ('title' in this)
            this.title = title;
        else
            this.label = title;
        if ('subtitle' in this)
            this.subtitle = subtitle;
        this.accessible_name = subtitle ? `${title}, ${subtitle}` : title;
    }
});

const DeskLinkIndicator = GObject.registerClass(
class DeskLinkIndicator extends SystemIndicator {
    _init() {
        super._init();
        this._icon = this._addIndicator();
        this._icon.icon_name = PHONE_ICON;
        this._icon.accessible_name = 'Phone connected';
        this._icon.visible = false;
        this.toggle = new DeskLinkToggle();
        this.quickSettingsItems.push(this.toggle);
    }

    showConnected(connected) {
        this._icon.visible = connected;
    }
});

/// The optional pill: the link glyph, a coral dot while live.
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
    // the pill offers "Unlock with <phone>" (DeskLink asks the phone, which
    // must approve). Quick Settings, notifications and every other action
    // are withdrawn while locked.
    enable() {
        this._selected = null;
        this._menuKey = null;
        this._notifications = new PhoneNotifications((method, args) => this._call(method, args));
        this._quietNotifications = true;

        this._indicator = new DeskLinkIndicator();
        this._indicator.toggle.connect('clicked', () => this._toggleClicked());
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);

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
        this._notifications.destroy();
        this._notifications = null;
        this._indicator.quickSettingsItems.forEach(item => item.destroy());
        this._indicator.destroy();
        this._indicator = null;
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
            this._quietNotifications = true;
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
        this._notifications?.destroy();
        if (this._indicator)
            this._sync();
    }

    _read(name, fallback) {
        return this._proxy?.get_cached_property(name)?.recursiveUnpack() ?? fallback;
    }

    _sync() {
        const locked = Main.sessionMode.isLocked;
        const running = !!this._proxy;
        const devices = this._read('Devices', []).filter(device => device.paired);
        const desk = this._read('Desk', {});
        const live = devices.filter(device => device.screen || device.terminals > 0);
        const selected = devices.find(device => device.id === this._selected) ??
            devices.find(device => device.ready) ??
            devices.find(device => device.reachable) ?? devices[0];
        this._selected = selected?.id ?? null;

        // Notifications: never while locked; the first read after (re)connecting
        // goes to the list without banners.
        if (running && !locked) {
            this._notifications.sync(this._read('Notifications', []), this._quietNotifications);
            this._quietNotifications = false;
        }

        // Quick Settings: everything, but not on the lock screen.
        const toggle = this._indicator.toggle;
        toggle.visible = running && !locked && devices.length > 0;
        toggle.checked = !!selected?.ready;
        toggle._setTitle(selected?.name ?? 'DeskLink', statusLine(selected));
        this._indicator.showConnected(running && !locked && devices.some(device => device.ready));
        if (!locked)
            this._fillMenu(toggle.menu, devices, selected, desk, live);

        this._syncPill(locked, running, devices, desk, live, selected);
    }

    _toggleClicked() {
        const selected = this._read('Devices', []).find(device => device.id === this._selected);
        if (selected?.ready)
            this._indicator.toggle.menu.open();
        else
            this._invoke('', 'reconnect');
    }

    _fillMenu(menu, devices, selected, desk, live) {
        const transfers = this._read('Transfers', []);
        // Rebuilt only when what it shows changed, so keyboard focus in an
        // open menu is not thrown away every second.
        const menuKey = JSON.stringify([devices, selected?.id, desk, transfers]);
        if (menuKey === this._menuKey)
            return;
        this._menuKey = menuKey;
        menu.setHeader(PHONE_ICON, selected?.name ?? 'DeskLink', statusLine(selected));
        menu.removeAll();

        if (devices.length > 1) {
            for (const device of devices) {
                const item = new PopupMenu.PopupMenuItem(`${device.name} · ${statusLine(device)}`);
                item.setOrnament(device.id === selected?.id
                    ? PopupMenu.Ornament.CHECK : PopupMenu.Ornament.NONE);
                item.connect('activate', () => {
                    this._selected = device.id;
                    this._menuKey = null;
                    this._sync();
                });
                menu.addMenuItem(item);
            }
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        }

        if (selected)
            menu.addMenuItem(this._actions(selected, desk));

        if (live.length > 0) {
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Live now'));
            for (const device of live)
                menu.addMenuItem(this._liveRow(device));
            menu.addAction('End Everything', () => this._invoke('', 'end-sessions'));
        }

        if (transfers.length > 0) {
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Recent files'));
            for (const file of transfers) {
                const words = file.incoming ? `from ${file.deviceName}` : `to ${file.deviceName}`;
                const item = new PopupMenu.PopupImageMenuItem(`${file.name} · ${words}`,
                    file.incoming ? 'folder-download-symbolic' : 'document-send-symbolic');
                item.connect('activate', () => this._call('OpenTransfer', [file.id]));
                menu.addMenuItem(item);
            }
        }

        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const handOff = menu.addAction('Lock & Hand Off', () => this._invoke('', 'lock-hand-off'));
        handOff.setSensitive(!!desk.handOff);
        menu.addAction('Open DeskLink', () => this._invoke(selected?.id ?? '', 'open'));
    }

    /// Ring, Lock, Send Files, Send clipboard and Keep Awake, in one row.
    _actions(device, desk) {
        const row = new PopupMenu.PopupBaseMenuItem({activate: false, hover: false, can_focus: false});
        const box = new St.BoxLayout({style_class: 'desklink-actions', x_expand: true});
        const can = action => device.ready && device.actions.includes(action);
        const add = (icon, label, name, action) => box.add_child(actionButton(icon, label, {
            name,
            enabled: can(action),
            onClick: () => this._invoke(device.id, action),
        }));
        add('audio-volume-high-symbolic', 'Ring', `Ring ${device.name}`, 'ring');
        add('system-lock-screen-symbolic', 'Lock', `Lock ${device.name}`, 'lock');
        add('document-send-symbolic', 'Files', `Send files to ${device.name}`, 'send-files');
        add('edit-paste-symbolic', 'Clipboard', `Send the clipboard to ${device.name}`,
            'send-clipboard');
        if (desk.keepAwake !== undefined) {
            box.add_child(actionButton('weather-clear-night-symbolic', 'Awake', {
                name: 'Keep this computer awake',
                checked: desk.keepAwake,
                onClick: () => this._invoke('', desk.keepAwake ? 'keep-awake-off' : 'keep-awake-on'),
            }));
        }
        row.add_child(box);
        return row;
    }

    /// "Pixel · viewing the screen · 2 terminals" with an End button.
    _liveRow(device) {
        const row = new PopupMenu.PopupBaseMenuItem({activate: false, hover: false, can_focus: false});
        row.add_child(new St.Label(centered({
            text: `${device.name} · ${sessionLine(device)}`,
            x_expand: true,
        })));
        const end = new St.Button({
            label: 'End',
            style_class: 'button desklink-end-button',
            can_focus: true,
            accessible_name: `End what ${device.name} is doing here`,
        });
        end.connect('clicked', () => this._invoke(device.id, 'end-sessions'));
        row.add_child(end);
        return row;
    }

    _syncPill(locked, running, devices, desk, live, selected) {
        const menu = this._button.menu;
        menu.removeAll();
        if (locked) {
            const unlockers = devices.filter(device => device.actions.includes('unlock-via-phone'));
            this._button.visible = running && unlockers.length > 0;
            this._button.showState(true, false, 'unlock with your phone');
            this._button.showLabel(unlockers.length === 1 ? `Unlock with ${unlockers[0].name}` : 'Unlock');
            for (const device of unlockers)
                menu.addAction(`Unlock with ${device.name}`, () => this._invoke(device.id, 'unlock-via-phone'));
            return;
        }
        this._button.visible = running && !!desk.pill && devices.length > 0;
        this._button.showLabel(null);
        this._button.showState(!!selected?.ready, live.length > 0,
            selected ? `${selected.name}, ${statusLine(selected)}` : 'no phone connected');
        const head = new PopupMenu.PopupMenuItem(
            selected ? `${selected.name} · ${statusLine(selected)}` : 'No phone connected',
            {reactive: false});
        menu.addMenuItem(head);
        for (const device of live)
            menu.addMenuItem(this._liveRow(device));
        menu.addAction('Open DeskLink', () => this._invoke(selected?.id ?? '', 'open'));
    }

    _call(method, args) {
        this._indicator?.toggle.menu.close();
        this._button?.menu.close();
        this._proxy?.[`${method}Remote`](...args, (_result, error) => {
            if (error)
                console.warn(`DeskLink: ${method} failed: ${error.message}`);
        });
    }

    _invoke(deviceId, action) {
        if (!this._proxy) {
            if (Main.sessionMode.isLocked)
                return;
            Shell.AppSystem.get_default().lookup_app(DESKTOP_FILE)?.activate();
            return;
        }
        this._call('Invoke', [deviceId, action]);
    }
}
