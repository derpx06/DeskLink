// SPDX-License-Identifier: GPL-2.0-or-later
//
// DeskLink in GNOME Shell.
//
// Quick Settings is the primary surface: a toggle named after the phone
// ("Connected · 82% · charging", or "last seen 14:02" while its link is down)
// whose menu is the complete remote for a paired phone: status chips, six
// quick actions, what is going on now, one section per area with every action
// and setting (model.js says what, menu.js draws it), and a footer. Security
// switches can only be turned off here; the app turns them on. A small status
// icon shows in the top bar while a phone is connected.
//
// The phones' notifications become GNOME notifications, and a call ringing on
// a phone a critical banner with Answer and Decline (notifications.js).
//
// An extra pill in the top bar (the link glyph, a coral dot while a phone is
// using this computer) is optional (DeskLink's Settings, off by default).
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
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

import {BatteryRing, fillMenu, shortStatus, statusLine, styleVariant} from './menu.js';
import {looksLikeLink} from './model.js';
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
    <property name="Snippets" type="aa{sv}" access="read"/>
    <property name="Activities" type="aa{sv}" access="read"/>
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
    <method name="SetSwitch">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="on" type="b" direction="in"/>
    </method>
    <method name="SendLink">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="link" type="s" direction="in"/>
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

// BEGIN wobble guard
// Keeps a window-wobble extension from hiding windows from the phone.
//
// "Compiz windows effect" (compiz-windows-effect@hermes83.github.com) wraps a
// window in a Clutter.DeformEffect while it is dragged, resized or
// maximised. Mutter's screen cast (the PipeWire stream DeskLink sends to the
// phone) draws such a window as *nothing* until the effect ends: the phone
// sees the window vanish for the whole drag (gnome-shell issue #7116; OBS and
// Discord lose it too). Measured on GNOME Shell 50.4: the dragged window is
// missing from every captured frame, in all three cursor modes, and from a
// Shell screenshot; any other window and a plain DeformEffect are fine.
//
// While a phone is looking at this screen, the effect is taken off the
// grabbed window as soon as the other extension adds it. The wobble is back
// the moment the phone stops looking. Only that one effect, by its name.
export const WOBBLE_EFFECT = 'wobbly-compiz-effect';

/// Takes the wobble effect off `window`'s actor, cleaning up through the
/// effect's own `destroy()` (it has timers and signal handlers of its own).
/// Returns whether there was one.
export function stripWobble(window) {
    const actor = window?.get_compositor_private?.();
    const effect = actor?.get_effect?.(WOBBLE_EFFECT);
    if (!effect)
        return false;
    try {
        effect.destroy();
    } catch (error) {
        // Not the shape this code knows: just detach it.
        actor.remove_effect(effect);
    }
    return true;
}

export class WobbleGuard {
    constructor(display = global.display) {
        this._display = display;
        this._id = 0;
    }

    /// True while a phone is viewing or controlling this screen.
    set sharing(on) {
        if (on && !this._id) {
            // After the other extension's own handler, which adds the effect.
            this._id = this._display.connect_after('grab-op-begin',
                (_display, window) => stripWobble(window));
        } else if (!on && this._id) {
            this._display.disconnect(this._id);
            this._id = 0;
        }
    }

    destroy() {
        this.sharing = false;
    }
}
// END wobble guard

// BEGIN activity helpers (pure; tested by tests/activity.test.mjs)
/// "4:32", or "1:05:09" from an hour; a countdown rounds up.
export function clockText(ms) {
    const seconds = Math.ceil(Math.max(0, ms) / 1000);
    const h = Math.floor(seconds / 3600), m = Math.floor(seconds / 60) % 60, s = seconds % 60;
    const two = n => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${m}:${two(s)}`;
}

/// The live part of an ongoing item ("4:32 left", "0:12 elapsed", "42%"), or
/// '' when it has none. `base` is on this computer's clock.
export function liveText(item, now) {
    if (item.base > 0) {
        if (item.countdown)
            return item.base - now <= 0 ? 'Done' : `${clockText(item.base - now)} left`;
        return `${clockText(now - item.base)} elapsed`;
    }
    return item.progress >= 0 ? `${item.progress}%` : '';
}

/// The items still worth showing: a countdown over for a minute is not.
export function liveItems(items, now) {
    return items.filter(item => !(item.countdown && item.base > 0 && now - item.base > 60000));
}
/// The pill's text cut to `max` characters (a navigation instruction can be
/// 240): the panel does not shorten a label, so it would push its neighbours.
export function chipLabel(text, max = 28) {
    const chars = Array.from(text);
    return chars.length > max ? `${chars.slice(0, max - 1).join('').trimEnd()}…` : text;
}
// END activity helpers

export default class DeskLinkExtension extends Extension {
    // "unlock-dialog" is in session-modes for one item: on the lock screen
    // the pill offers "Unlock with <phone>" (DeskLink asks the phone, which
    // must approve). Quick Settings, notifications and every other action
    // are withdrawn while locked.
    enable() {
        this._selected = null;
        this._wobble = new WobbleGuard();
        this._notifications = new PhoneNotifications((method, args) => this._call(method, args));
        this._calls = new PhoneCalls((method, args) => this._call(method, args));
        this._quietNotifications = true;

        this._indicator = new DeskLinkIndicator(this.path);
        this._indicator.toggle.connect('clicked', () => this._toggleClicked());
        this._ring = new BatteryRing();
        this._indicator.toggle.menu.addHeaderSuffix(this._ring);
        // Durations and call timers move while a menu is open.
        this._qs = this._menuState();
        this._watchMenu(this._indicator.toggle.menu, this._qs);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);

        this._button = new DeskLinkButton();
        this._pill = this._menuState();
        this._watchMenu(this._button.menu, this._pill);
        Main.panel.addToStatusArea(this.uuid, this._button);

        this._modeId = Main.sessionMode.connect('updated', () => this._sync());
        this._sync();

        this._watchId = Gio.bus_watch_name(Gio.BusType.SESSION, BUS_NAME,
            Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => this._disconnect());
    }

    disable() {
        for (const state of [this._qs, this._pill]) {
            if (state?.tickId)
                GLib.source_remove(state.tickId);
            if (state)
                state.tickId = 0;
        }
        this._stopChipTimer();
        Gio.bus_unwatch_name(this._watchId);
        this._watchId = 0;
        Main.sessionMode.disconnect(this._modeId);
        this._modeId = 0;
        this._disconnect();
        this._wobble.destroy();
        this._wobble = null;
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
        // A phone looking at this screen: no wobbling windows, they vanish from the picture (see the wobble guard above).
        if (this._wobble)
            this._wobble.sharing = running && devices.some(device => device.screen === 'view' || device.screen === 'control');
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
            this._fillMenu(toggle.menu, this._qs, devices, selected, desk);

        this._syncPill(locked, running, devices, desk, live, selected);
    }

    _toggleClicked() {
        const selected = this._read('Devices', []).find(device => device.id === this._selected);
        if (selected?.ready)
            this._indicator.toggle.menu.open();
        else
            this._invoke('', 'reconnect');
    }

    _menuState() {
        return {key: null, tickers: [], bucketUpdate: null, transfersUpdate: null, seen: new Set(),
            openSection: null, items: new Map(), focus: null, link: false, tickId: 0};
    }

    /// Seconds tick while the menu is open, and the clipboard is looked at
    /// for "Send copied link".
    _watchMenu(menu, state) {
        menu.connect('open-state-changed', (_menu, open) => {
            if (state.tickId)
                GLib.source_remove(state.tickId);
            state.tickId = open ? GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 1, () => {
                state.tickers.forEach(tick => tick());
                return GLib.SOURCE_CONTINUE;
            }) : 0;
            if (open)
                this._lookAtClipboard(state);
            else
                state.focus = null; // a closed menu keeps no focus to restore
        });
    }

    _lookAtClipboard(state) {
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_clipboard, text) => {
            const link = looksLikeLink(text);
            if (link === state.link || !this._proxy)
                return;
            state.link = link;
            state.key = null;
            this._sync();
        });
    }

    _fillMenu(menu, state, devices, selected, desk) {
        const transfers = this._read('Transfers', []);
        const snippets = this._read('Snippets', []);
        const activities = this._read('Activities', []);
        // Rebuilt only when what it shows changed, so keyboard focus in an
        // open menu is not thrown away every second. The Bucket's and the
        // transfers' progress and the round trip change all the time: the
        // first two are updated in place, the third is rounded.
        const steady = devices.map(device => ({
            ...device,
            bucket: !!device.bucket,
            rttMs: Math.round((device.rttMs ?? -1) / 25),
            transfers: (device.transfers ?? []).map(t => [t.name, t.incoming, t.paused, t.lane, t.fraction > 0]),
        }));
        const key = JSON.stringify([steady, selected?.id, desk, transfers, snippets, styleVariant(), state.link,
            activities.map(item => [item.id, item.title])]);
        if (key === state.key) {
            state.bucketUpdate?.(selected?.bucket);
            state.transfersUpdate?.(selected?.transfers ?? []);
            return;
        }
        state.key = key;
        state.tickers = [];
        state.bucketUpdate = null;
        state.transfersUpdate = null;
        state.items = new Map();
        const shown = new Set();
        fillMenu(menu, {
            devices, selected, desk, transfers, snippets, activities,
            ring: menu === this._indicator?.toggle.menu ? this._ring : null,
            linkInClipboard: state.link,
            openSection: state.openSection,
            sectionOpened: (id, open) => {
                if (open) {
                    state.openSection = id;
                } else {
                    if (state.openSection === id)
                        state.openSection = null;
                    // Rows of a section that closed are not where focus belongs.
                    if (state.focus?.startsWith(`${id}:`))
                        state.focus = null;
                }
            },
            activityText: item => this._activityText(item),
            invoke: (id, action) => this._invoke(id, action),
            call: (method, args) => this._call(method, args),
            setSwitch: (id, switchKey, on) => this._setSwitch(id, switchKey, on),
            sendLink: id => this._sendLink(id),
            select: id => {
                this._selected = id;
                state.key = null;
                this._sync();
            },
            tick: fn => state.tickers.push(fn),
            onBucket: fn => (state.bucketUpdate = fn),
            onTransfers: fn => (state.transfersUpdate = fn),
            // Keyboard focus (or the hovered row) goes back to the same row.
            track: (item, itemKey) => {
                state.items.set(itemKey, item);
                item.connect('key-focus-in', () => (state.focus = itemKey));
            },
            // New cards fade in (instantly when animations are off).
            appear: (actor, appearKey) => {
                shown.add(appearKey);
                if (state.seen.has(appearKey))
                    return;
                actor.opacity = 0;
                actor.ease({opacity: 255, duration: 220, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            },
        });
        state.seen = shown;
        const focused = menu.isOpen && state.focus ? state.items.get(state.focus) : null;
        if (focused?.mapped)
            focused.grabKeyFocus?.();
    }

    /// A switch in the menu: the app decides (a security one only goes off).
    /// The menu stays open; a refusal puts the menu back as the app has it.
    _setSwitch(deviceId, key, on) {
        const proxy = this._proxy;
        if (!proxy)
            return;
        const done = (_result, error) => {
            if (!error)
                return;
            console.warn(`DeskLink: ${key} failed: ${error.message}`);
            this._qs.key = null;
            this._pill.key = null;
            this._sync();
        };
        // An app older than interface 9 only knows two of them, as actions.
        if (this._read('Version', 0) < 9) {
            const old = {keepAwake: 'keep-awake', clipboard: 'clipboard'}[key];
            if (old)
                proxy.InvokeRemote('', `${old}-${on ? 'on' : 'off'}`, done);
            return;
        }
        proxy.SetSwitchRemote(deviceId, key, on, done);
    }

    /// "Send copied link": the clipboard's text, which the app checks again.
    _sendLink(deviceId) {
        St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_clipboard, text) => {
            if (!looksLikeLink(text) || !this._proxy)
                return;
            this._call('SendLink', [deviceId, text.trim()]);
        });
    }

    _activityText(item) {
        const text = liveText(item, Date.now());
        return text || (item.kind ?? '');
    }

    _syncPill(locked, running, devices, desk, live, selected) {
        const menu = this._button.menu;
        if (locked) {
            menu.removeAll();
            this._pill.key = null;
            const unlockers = devices.filter(device => device.actions.includes('unlock-via-phone'));
            this._button.visible = running && unlockers.length > 0;
            this._chipItems = [];
            this._stopChipTimer();
            this._button.showState(true, false, 'unlock with your phone');
            this._button.showLabel(unlockers.length === 1 ? `Unlock with ${unlockers[0].name}` : 'Unlock');
            for (const device of unlockers)
                menu.addAction(`Unlock with ${device.name}`, () => this._invoke(device.id, 'unlock-via-phone'));
            return;
        }
        // Ongoing items on the phone (timers, downloads, navigation): the pill
        // shows the first one's live countdown or progress, even when the
        // pill is otherwise off, and goes when nothing is ongoing.
        const items = running ? liveItems(this._read('Activities', []), Date.now()) : [];
        this._chipItems = items;
        this._button.visible = running && (!!desk.pill || items.length > 0) && devices.length > 0;
        this._button.showLabel(this._chipText());
        this._button.showState(!!selected?.ready, live.length > 0,
            selected ? `${selected.name}, ${statusLine(selected)}` : 'no phone connected');
        if (items.length > 0)
            this._startChipTimer();
        else
            this._stopChipTimer();
        // The same complete menu as Quick Settings (built only while shown).
        if (this._button.visible)
            this._fillMenu(menu, this._pill, devices, selected, desk);
        else
            this._pill.key = null;
    }

    /// The pill's text: the first ongoing item and its live part.
    _chipText() {
        const item = this._chipItems?.[0];
        if (!item)
            return null;
        const live = liveText(item, Date.now());
        const name = item.title || item.app;
        return chipLabel(live ? `${name} ${live}` : name);
    }

    /// One tick a second, only while something is ongoing.
    _startChipTimer() {
        if (this._chipId)
            return;
        this._chipId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 1, () => {
            const before = this._chipItems?.length ?? 0;
            this._chipItems = liveItems(this._chipItems ?? [], Date.now());
            if (this._chipItems.length !== before) {
                // A countdown ran out: the whole pill is worked out again.
                this._chipId = 0;
                this._sync();
                return GLib.SOURCE_REMOVE;
            }
            this._button?.showLabel(this._chipText());
            return GLib.SOURCE_CONTINUE;
        });
    }

    _stopChipTimer() {
        if (this._chipId)
            GLib.source_remove(this._chipId);
        this._chipId = 0;
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
