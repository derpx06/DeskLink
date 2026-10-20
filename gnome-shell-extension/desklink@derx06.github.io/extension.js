// SPDX-License-Identifier: GPL-2.0-or-later
//
// DeskLink in the GNOME top bar: a Quick Settings tile listing paired phones,
// their battery, how they are connected and what they are doing on this
// computer, with the actions DeskLink offers for each right now.
//
// Everything comes from the running app over D-Bus
// (derx06.desklink.com.Shell1 at /derx06/desklink/com/Shell); nothing is shown
// while DeskLink is not running.

import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

const BUS_NAME = 'derx06.desklink.com';
const OBJECT_PATH = '/derx06/desklink/com/Shell';
const DESKTOP_FILE = 'derx06.desklink.com.desktop';

const ShellProxy = Gio.DBusProxy.makeProxyWrapper(`<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
  </interface>
</node>`);

// Per-device actions, in menu order. Anything else the app lists is ignored.
const ACTIONS = [
    ['ring', 'Ring'],
    ['send-files', 'Send files…'],
    ['new-terminal', 'New terminal'],
    ['end-sessions', 'End sessions'],
];

const PATHS = {
    lan: 'Local network',
    direct: 'Over the internet',
    turn: 'Through a relay server',
};

function appIcon() {
    return Gio.ThemedIcon.new_from_names(['derx06.desklink.com-symbolic', 'phone-symbolic']);
}

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
        parts.push('Controlling the screen');
    else if (device.screen === 'view')
        parts.push('Viewing the screen');
    if (device.terminals > 0)
        parts.push(device.terminals === 1 ? '1 terminal' : `${device.terminals} terminals`);
    return parts.join(' · ');
}

const DeskLinkToggle = GObject.registerClass(
class DeskLinkToggle extends QuickMenuToggle {
    _init() {
        super._init({title: 'DeskLink', gicon: appIcon(), toggleMode: false});
        // A status tile, not a switch: clicking it opens the menu.
        this.connect('clicked', () => this.menu.open());
    }
});

export default class DeskLinkExtension extends Extension {
    enable() {
        this._indicator = new SystemIndicator();
        this._icon = this._indicator._addIndicator();
        this._icon.gicon = appIcon();
        this._toggle = new DeskLinkToggle();
        this._indicator.quickSettingsItems.push(this._toggle);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
        this._sync();

        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => this._disconnect());
    }

    disable() {
        Gio.bus_unwatch_name(this._watchId);
        this._watchId = 0;
        this._disconnect();
        this._indicator.quickSettingsItems.forEach(item => item.destroy());
        this._indicator.destroy();
        this._indicator = null;
        this._icon = null;
        this._toggle = null;
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
        if (this._indicator)
            this._sync();
    }

    _sync() {
        const running = !!this._proxy;
        const devices = (this._proxy?.get_cached_property('Devices')?.recursiveUnpack() ?? [])
            .filter(device => device.paired);
        const busy = devices.some(device => device.screen || device.terminals > 0);
        const shown = devices.find(device => device.ready) ??
            devices.find(device => device.reachable);
        const subtitle = shown ? title(shown) : 'No phone connected';

        this._icon.visible = running && (busy || !!shown);
        this._toggle.visible = running;
        this._toggle.checked = devices.some(device => device.ready);
        this._toggle.subtitle = subtitle;

        const menu = this._toggle.menu;
        menu.setHeader(appIcon(), 'DeskLink', subtitle);
        menu.removeAll();
        for (const device of devices) {
            const name = new PopupMenu.PopupMenuItem(title(device), {reactive: false});
            name.label.add_style_class_name('desklink-device-name');
            menu.addMenuItem(name);
            for (const line of [pathLine(device), sessionLine(device)].filter(Boolean))
                menu.addMenuItem(new PopupMenu.PopupMenuItem(line, {reactive: false}));
            for (const [action, label] of ACTIONS) {
                if (device.actions.includes(action))
                    menu.addAction(label, () => this._invoke(device.id, action));
            }
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        }
        menu.addAction('Open DeskLink', () => this._invoke('', 'open'));
        menu.addAction('Pair a new phone', () => this._invoke('', 'pair-new'));
    }

    _invoke(deviceId, action) {
        Main.panel.closeQuickSettings();
        if (!this._proxy) {
            Shell.AppSystem.get_default().lookup_app(DESKTOP_FILE)?.activate();
            return;
        }
        this._proxy.InvokeRemote(deviceId, action, (_result, error) => {
            if (error)
                console.warn(`DeskLink: ${action} failed: ${error.message}`);
        });
    }
}
