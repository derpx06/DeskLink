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
// The phones' notifications become GNOME notifications, and a call ringing on
// a phone a critical banner with Answer and Decline (notifications.js).
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

import {BatteryRing, fillMenu, sessionLine, shortStatus, statusLine, styleVariant} from './menu.js';
import {PhoneCalls, PhoneNotifications} from './notifications.js';

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
    <method name="Hello">
      <arg name="version" type="u" direction="in"/>
      <arg name="capabilities" type="as" direction="in"/>
    </method>
  </interface>
</node>`);

function centered(params) {
    return {y_align: Clutter.ActorAlign.CENTER, ...params};
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

/// The top bar: DeskLink's link glyph while a phone is connected, with a
/// coral dot while a phone is using this computer.
const DeskLinkIndicator = GObject.registerClass(
class DeskLinkIndicator extends SystemIndicator {
    _init(path) {
        super._init();
        this._icon = this._addIndicator();
        this._icon.gicon = Gio.FileIcon.new(Gio.File.new_for_path(`${path}/desklink-symbolic.svg`));
        this._icon.accessible_name = 'Phone connected';
        this._icon.visible = false;
        this._dot = new St.Widget(centered({style_class: 'desklink-indicator-dot', visible: false}));
        this.add_child(this._dot);
        this.toggle = new DeskLinkToggle();
        this.quickSettingsItems.push(this.toggle);
    }

    showConnected(connected, live) {
        this._icon.visible = connected;
        this._dot.visible = connected && live;
        this._icon.accessible_name = live ? 'Phone connected, using this computer' : 'Phone connected';
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
        this._calls = new PhoneCalls((method, args) => this._call(method, args));
        this._quietNotifications = true;

        this._indicator = new DeskLinkIndicator(this.path);
        this._indicator.toggle.connect('clicked', () => this._toggleClicked());
        this._ring = new BatteryRing();
        this._indicator.toggle.menu.addHeaderSuffix(this._ring);
        // Durations and call timers move while the menu is open.
        this._tickers = [];
        this._seen = new Set();
        this._tickId = 0;
        this._indicator.toggle.menu.connect('open-state-changed', (_menu, open) => {
            if (this._tickId)
                GLib.source_remove(this._tickId);
            this._tickId = open ? GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 1, () => {
                this._tickers.forEach(tick => tick());
                return GLib.SOURCE_CONTINUE;
            }) : 0;
        });
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
        if (this._tickId)
            GLib.source_remove(this._tickId);
        this._tickId = 0;
        Gio.bus_unwatch_name(this._watchId);
        this._watchId = 0;
        Main.sessionMode.disconnect(this._modeId);
        this._modeId = 0;
        this._disconnect();
        this._notifications.destroy();
        this._notifications = null;
        this._calls.destroy();
        this._calls = null;
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
            // Tells DeskLink this extension shows the phones' notifications
            // and ringing calls, so the app stops showing its own (an older
            // extension does not say so, and the app keeps them).
            proxy.HelloRemote(1, ['notifications', 'quick-settings', 'calls'], (_result, error) => {
                if (error)
                    console.warn(`DeskLink: Hello failed: ${error.message}`);
            });
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
        this._calls?.destroy();
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
        // A ringing call: a critical banner, never on the lock screen (it
        // comes back on unlock if the phone still rings).
        this._calls.sync(running && !locked ? devices : []);

        // Quick Settings: everything, but not on the lock screen.
        const toggle = this._indicator.toggle;
        toggle.visible = running && !locked && devices.length > 0;
        toggle.checked = !!selected?.ready;
        toggle._setTitle(selected?.name ?? 'DeskLink', shortStatus(selected));
        this._indicator.showConnected(running && !locked && devices.some(device => device.ready),
            live.length > 0);
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

    _fillMenu(menu, devices, selected, desk, live_) {
        const transfers = this._read('Transfers', []);
        // Rebuilt only when what it shows changed, so keyboard focus in an
        // open menu is not thrown away every second.
        const menuKey = JSON.stringify([devices, selected?.id, desk, transfers, styleVariant()]);
        if (menuKey === this._menuKey)
            return;
        this._menuKey = menuKey;
        this._tickers = [];
        const shown = new Set();
        fillMenu(menu, {
            devices, selected, desk, transfers,
            ring: this._ring,
            invoke: (id, action) => this._invoke(id, action),
            call: (method, args) => this._call(method, args),
            select: id => {
                this._selected = id;
                this._menuKey = null;
                this._sync();
            },
            tick: fn => this._tickers.push(fn),
            // New cards fade in (instantly when animations are off).
            appear: (actor, key) => {
                shown.add(key);
                if (this._seen.has(key))
                    return;
                actor.opacity = 0;
                actor.ease({opacity: 255, duration: 220, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            },
        });
        this._seen = shown;
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
