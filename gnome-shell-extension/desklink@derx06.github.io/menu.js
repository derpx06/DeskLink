// SPDX-License-Identifier: GPL-2.0-or-later
//
// The Quick Settings menu's content: a header with the phone and a battery
// ring, the phone switcher, a call or offline card, a grid of actions, the
// phone's music, what is live on this computer, and recent files. Stock
// St/PopupMenu widgets and the shell's own button and quick-toggle styles,
// so it follows light/dark, the accent colour and high contrast; only the
// mint and coral status marks are DeskLink's own.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const MINT = [0x2f / 255, 0xbf / 255, 0x8f / 255];
const CORAL = [0xe5 / 255, 0x53 / 255, 0x3d / 255];
const ROUTE = {lan: 'Wi-Fi', direct: 'Internet', turn: 'Relay', relay: 'Relay'};
const IMAGE = /\.(png|jpe?g|gif|webp|bmp|svg|avif|heic)$/i;

export function lastSeen(device) {
    if (!device.lastSeen)
        return '';
    const seen = GLib.DateTime.new_from_unix_local(device.lastSeen);
    const now = GLib.DateTime.new_now_local();
    const today = seen.get_ymd().join() === now.get_ymd().join();
    return `last seen ${seen.format(today ? '%H:%M' : '%-d %b %H:%M')}`;
}

/// "Connected · Wi-Fi · 82% ⚡", "last seen 14:02", "Connecting…".
export function statusLine(device) {
    if (!device)
        return 'No phone connected';
    if (device.ready) {
        const parts = ['Connected'];
        if (ROUTE[device.path])
            parts.push(ROUTE[device.path]);
        if (device.battery >= 0)
            parts.push(`${device.battery}%${device.charging ? ' ⚡' : ''}`);
        return parts.join(' · ');
    }
    if (device.reachable)
        return 'Connecting…';
    return lastSeen(device) || 'Not connected';
}

/// "Wi-Fi · 82%": the toggle's short subtitle (its title is the phone).
export function shortStatus(device) {
    if (!device?.ready)
        return statusLine(device);
    const parts = [];
    if (ROUTE[device.path])
        parts.push(ROUTE[device.path]);
    if (device.battery >= 0)
        parts.push(`${device.battery}%`);
    return parts.join(' · ') || 'Connected';
}

/// The shell's light or dark style, for the card surfaces.
export function styleVariant() {
    return Main.getStyleVariant?.() === 'light' ? 'light' : 'dark';
}

/// "Viewing · 2 terminals": a live card's short line.
function liveWords(device) {
    const parts = [];
    if (device.screen === 'control')
        parts.push('Controlling');
    else if (device.screen === 'view')
        parts.push('Viewing');
    if (device.terminals > 0)
        parts.push(device.terminals === 1 ? '1 terminal' : `${device.terminals} terminals`);
    return parts.join(' · ');
}

/// "viewing the screen · 2 terminals".
export function sessionLine(device) {
    const parts = [];
    if (device.screen === 'control')
        parts.push('controlling the screen');
    else if (device.screen === 'view')
        parts.push('viewing the screen');
    else if (device.screen === 'glide')
        parts.push('this mouse and keyboard are on it · Esc+Super to return');
    if (device.terminals > 0)
        parts.push(device.terminals === 1 ? '1 terminal' : `${device.terminals} terminals`);
    return parts.join(' · ');
}

/// "just now", "12 min", "1 h 05 min" since `unixSeconds`.
function duration(unixSeconds) {
    const minutes = Math.max(0, Math.floor((Date.now() / 1000 - unixSeconds) / 60));
    if (minutes < 1)
        return 'just now';
    if (minutes < 60)
        return `${minutes} min`;
    return `${Math.floor(minutes / 60)} h ${String(minutes % 60).padStart(2, '0')} min`;
}

/// "1:05", "1:02:03" since `unixMillis`.
function timer(unixMillis) {
    const seconds = Math.max(0, Math.floor((Date.now() - unixMillis) / 1000));
    const [h, m, s] = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60];
    const pad = n => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function label(text, styleClass, params = {}) {
    const actor = new St.Label({text, style_class: styleClass, y_align: Clutter.ActorAlign.CENTER, ...params});
    actor.clutter_text.ellipsize = 3; // Pango.EllipsizeMode.END
    return actor;
}

function dim(actor) {
    actor.opacity = 178;
    return actor;
}

function iconButton(icon, name, onClick, extraClass = '') {
    const button = new St.Button({
        style_class: `button desklink-round ${extraClass}`,
        child: new St.Icon({icon_name: icon, x_align: Clutter.ActorAlign.CENTER, y_align: Clutter.ActorAlign.CENTER}),
        can_focus: true,
        accessible_name: name,
        y_align: Clutter.ActorAlign.CENTER,
    });
    button.connect('clicked', onClick);
    return button;
}

function textButton(text, onClick, extraClass = '') {
    const button = new St.Button({
        style_class: `button desklink-pill-button ${extraClass}`,
        label: text,
        can_focus: true,
        y_align: Clutter.ActorAlign.CENTER,
    });
    button.connect('clicked', onClick);
    return button;
}

/// A non-activating row holding custom content.
function row(child, styleClass = '') {
    const item = new PopupMenu.PopupBaseMenuItem({
        activate: false, hover: false, can_focus: false, style_class: `desklink-row ${styleClass}`,
    });
    item.add_child(child);
    return item;
}

function box(params = {}) {
    return new St.BoxLayout({x_expand: true, ...params});
}

function column(params = {}) {
    return new St.BoxLayout({vertical: true, x_expand: true, y_align: Clutter.ActorAlign.CENTER, ...params});
}

/// The battery as a ring around its percentage: mint, coral when low.
export const BatteryRing = GObject.registerClass(
class BatteryRing extends St.Widget {
    _init() {
        super._init({
            layout_manager: new Clutter.BinLayout(),
            style_class: 'desklink-battery',
            x_expand: true,
            x_align: Clutter.ActorAlign.END,
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._level = -1;
        this._charging = false;
        this._area = new St.DrawingArea({style_class: 'desklink-battery-ring', x_expand: true, y_expand: true});
        this._area.connect('repaint', area => this._repaint(area));
        this._label = new St.Label({style_class: 'desklink-battery-label', x_align: Clutter.ActorAlign.CENTER, y_align: Clutter.ActorAlign.CENTER});
        this.add_child(this._area);
        this.add_child(this._label);
    }

    set(level, charging) {
        this.visible = level >= 0;
        if (level === this._level && charging === this._charging)
            return;
        this._level = level;
        this._charging = charging;
        this._label.text = `${level}`;
        this.accessible_name = `Battery ${level}%${charging ? ', charging' : ''}`;
        this._area.queue_repaint();
    }

    _repaint(area) {
        const cr = area.get_context();
        const [width, height] = area.get_surface_size();
        const fg = area.get_theme_node().get_foreground_color();
        const lineWidth = Math.max(2, Math.round(width / 11));
        const radius = Math.min(width, height) / 2 - lineWidth;
        cr.setLineWidth(lineWidth);
        cr.setLineCap(Cairo.LineCap.ROUND);
        cr.setSourceRGBA(fg.red / 255, fg.green / 255, fg.blue / 255, 0.18);
        cr.arc(width / 2, height / 2, radius, 0, 2 * Math.PI);
        cr.stroke();
        if (this._level > 0) {
            const [r, g, b] = this._level <= 20 && !this._charging ? CORAL : MINT;
            cr.setSourceRGBA(r, g, b, 1);
            const start = -Math.PI / 2;
            cr.arc(width / 2, height / 2, radius, start, start + 2 * Math.PI * this._level / 100);
            cr.stroke();
        }
        cr.$dispose();
    }
});

/// One rounded tile of the action grid: icon over a short label.
function tile(icon, text, {name = text, checked = null, enabled = true, variant = '', onClick}) {
    const content = new St.BoxLayout({vertical: true, x_expand: true, x_align: Clutter.ActorAlign.CENTER, style_class: 'desklink-tile-content'});
    content.add_child(new St.Icon({icon_name: icon, style_class: 'desklink-tile-icon', x_align: Clutter.ActorAlign.CENTER}));
    content.add_child(new St.Label({text, style_class: 'desklink-tile-label', x_align: Clutter.ActorAlign.CENTER}));
    const button = new St.Button({
        child: content,
        style_class: `button desklink-tile ${variant}`,
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

/// A file's picture: the image itself, else its type's icon.
function fileIcon(file) {
    const path = file.id;
    if (IMAGE.test(file.name) && GLib.file_test(path, GLib.FileTest.EXISTS))
        return Gio.FileIcon.new(Gio.File.new_for_path(path));
    const [type] = Gio.content_type_guess(file.name, null);
    return Gio.content_type_get_symbolic_icon(type);
}

/// Builds the menu. `ctx`: devices, selected, desk, transfers,
/// invoke(id, action), call(method, args), select(id), appear(actor, key),
/// tick(fn) (run every second while open).
export function fillMenu(menu, ctx) {
    const {devices, selected, desk, transfers, invoke} = ctx;
    const variant = `desklink-${styleVariant()}`;
    const can = (device, action) => device?.actions.includes(action);
    menu.setHeader('phone-symbolic', selected?.name ?? 'DeskLink', statusLine(selected));
    ctx.ring.set(selected?.ready ? selected.battery : -1, !!selected?.charging);
    menu.removeAll();

    // Phones: one chip each, the shown one checked.
    if (devices.length > 1) {
        const chips = box({style_class: 'desklink-chips'});
        for (const device of devices) {
            const chip = new St.Button({
                style_class: `button desklink-chip ${variant}`,
                toggle_mode: true,
                checked: device.id === selected?.id,
                can_focus: true,
                accessible_name: `${device.name}, ${statusLine(device)}`,
            });
            const content = box({style_class: 'desklink-chip-content'});
            const dot = new St.Widget({style_class: `desklink-dot ${device.ready ? 'mint' : ''}`, y_align: Clutter.ActorAlign.CENTER});
            content.add_child(dot);
            content.add_child(label(device.name, 'desklink-chip-label'));
            chip.child = content;
            chip.connect('clicked', () => ctx.select(device.id));
            chips.add_child(chip);
        }
        menu.addMenuItem(row(chips));
    }

    if (!selected) {
        menu.addMenuItem(row(dim(label('Pair a phone in DeskLink to see it here.', 'desklink-empty'))));
        menu.addAction('Open DeskLink', () => invoke('', 'open'));
        return;
    }

    // Not connected: when it was last here, and Reconnect.
    if (!selected.ready) {
        const card = box({style_class: `desklink-card ${variant}`});
        card.add_child(new St.Icon({icon_name: 'network-offline-symbolic', style_class: 'desklink-card-icon'}));
        const words = column();
        words.add_child(label(selected.reachable ? 'Connecting…' : 'Not connected', 'desklink-card-title'));
        words.add_child(dim(label(lastSeen(selected) || 'Waiting for the phone', 'desklink-card-subtitle')));
        card.add_child(words);
        card.add_child(textButton('Reconnect', () => invoke('', 'reconnect')));
        ctx.appear(card, `offline:${selected.id}`);
        menu.addMenuItem(row(card));
    }

    // A call: Answer/Decline while ringing, the time and End while on it.
    const call = selected.call;
    if (call) {
        const ringing = call.state === 'ringing' && call.incoming;
        const card = box({style_class: `desklink-card ${variant} desklink-call ${ringing ? 'ringing' : ''}`});
        card.add_child(new St.Icon({icon_name: 'call-start-symbolic', style_class: 'desklink-call-icon', y_align: Clutter.ActorAlign.CENTER}));
        const words = column();
        words.add_child(label(call.caller, 'desklink-card-title'));
        const detail = dim(label('', 'desklink-card-subtitle'));
        const describe = () => {
            if (ringing)
                return call.silenced ? 'Incoming call · silenced' : 'Incoming call';
            if (call.state === 'talking' && call.startedAt)
                return `On a call · ${timer(call.startedAt)}${call.muted ? ' · muted' : ''}`;
            return call.state === 'held' ? 'On hold' : 'Calling…';
        };
        detail.text = describe();
        ctx.tick(() => (detail.text = describe()));
        words.add_child(detail);
        card.add_child(words);
        if (ringing) {
            if (can(selected, 'call-decline'))
                card.add_child(iconButton('call-stop-symbolic', 'Decline', () => invoke(selected.id, 'call-decline'), 'desklink-decline'));
            if (can(selected, 'call-answer'))
                card.add_child(iconButton('call-start-symbolic', 'Answer', () => invoke(selected.id, 'call-answer'), 'desklink-answer'));
        } else {
            if (can(selected, 'call-mute'))
                card.add_child(iconButton('microphone-sensitivity-muted-symbolic', 'Mute', () => invoke(selected.id, 'call-mute'), variant));
            if (can(selected, 'call-unmute'))
                card.add_child(iconButton('audio-input-microphone-symbolic', 'Unmute', () => invoke(selected.id, 'call-unmute'), variant));
            if (can(selected, 'call-end'))
                card.add_child(iconButton('call-stop-symbolic', 'End call', () => invoke(selected.id, 'call-end'), 'desklink-decline'));
        }
        ctx.appear(card, `call:${call.id}:${call.state}`);
        menu.addMenuItem(row(card));
    }

    // The actions, three to a row.
    const ready = !!selected.ready;
    const tiles = [
        tile('audio-volume-high-symbolic', 'Ring', {variant, name: `Ring ${selected.name}`, enabled: ready && can(selected, 'ring'), onClick: () => invoke(selected.id, 'ring')}),
        tile('system-lock-screen-symbolic', 'Lock', {variant, name: `Lock ${selected.name}`, enabled: ready && can(selected, 'lock'), onClick: () => invoke(selected.id, 'lock')}),
        tile('document-send-symbolic', 'Files', {variant, name: `Send files to ${selected.name}`, enabled: ready && can(selected, 'send-files'), onClick: () => invoke(selected.id, 'send-files')}),
        tile('edit-paste-symbolic', 'Clipboard', {variant, name: `Send the clipboard to ${selected.name}`, enabled: ready && can(selected, 'send-clipboard'), onClick: () => invoke(selected.id, 'send-clipboard')}),
    ];
    if (desk.keepAwake !== undefined) {
        tiles.push(tile('weather-clear-night-symbolic', 'Awake', {
            variant,
            name: 'Keep this computer awake',
            checked: desk.keepAwake,
            onClick: () => invoke('', desk.keepAwake ? 'keep-awake-off' : 'keep-awake-on'),
        }));
    }
    tiles.push(tile('send-to-symbolic', 'Hand Off', {
        variant,
        name: 'Lock this computer and continue on the phone',
        enabled: !!desk.handOff,
        onClick: () => invoke('', 'lock-hand-off'),
    }));
    const grid = column({style_class: 'desklink-grid'});
    for (let i = 0; i < tiles.length; i += 3) {
        const line = box({style_class: 'desklink-grid-row'});
        tiles.slice(i, i + 3).forEach(t => line.add_child(t));
        grid.add_child(line);
    }
    menu.addMenuItem(row(grid));

    // The phone's music.
    const media = ready ? selected.media : null;
    if (media) {
        const line = box({style_class: 'desklink-media'});
        line.add_child(new St.Icon({icon_name: 'audio-x-generic-symbolic', style_class: `desklink-art ${variant}`, y_align: Clutter.ActorAlign.CENTER}));
        const words = column();
        words.add_child(label(media.title, 'desklink-card-title'));
        if (media.artist)
            words.add_child(dim(label(media.artist, 'desklink-card-subtitle')));
        line.add_child(words);
        line.add_child(iconButton(media.playing ? 'media-playback-pause-symbolic' : 'media-playback-start-symbolic',
            media.playing ? 'Pause' : 'Play', () => invoke(selected.id, 'media-play-pause'), variant));
        line.add_child(iconButton('media-skip-forward-symbolic', 'Next track', () => invoke(selected.id, 'media-next'), variant));
        menu.addMenuItem(row(line));
    }

    // The Bucket syncing: what, how fast, how long; opens the Bucket page.
    const bucket = selected.bucket;
    if (bucket) {
        const item = new PopupMenu.PopupBaseMenuItem({style_class: 'desklink-row desklink-bucket'});
        const words = column({style_class: 'desklink-bucket-words'});
        const head = box();
        head.add_child(new St.Icon({icon_name: 'folder-symbolic', style_class: 'desklink-bucket-icon', y_align: Clutter.ActorAlign.CENTER}));
        const line = label('', 'desklink-card-title');
        head.add_child(line);
        words.add_child(head);
        const detail = dim(label('', 'desklink-card-subtitle'));
        words.add_child(detail);
        const track = new St.Widget({style_class: 'desklink-progress', x_expand: true, layout_manager: new Clutter.BinLayout()});
        const fill = new St.Widget({style_class: 'desklink-progress-fill', x_align: Clutter.ActorAlign.START, y_expand: true});
        track.add_child(fill);
        words.add_child(track);
        item.add_child(words);
        let fraction = 0;
        const draw = () => (fill.width = Math.round(track.width * fraction));
        track.connect('notify::width', draw);
        const show = now => {
            if (!now)
                return;
            line.text = now.line;
            detail.text = now.detail;
            fraction = Math.min(1, Math.max(0, now.fraction ?? 0));
            item.accessible_name = `Bucket: ${now.line}, ${now.detail}, ${Math.round(fraction * 100)}%`;
            draw();
        };
        show(bucket);
        ctx.onBucket?.(show);
        item.connect('activate', () => invoke(selected.id, 'open-bucket'));
        ctx.appear(item, `bucket:${selected.id}`);
        menu.addMenuItem(item);
    }

    // What is live on this computer.
    const live = devices.filter(device => device.screen || device.terminals > 0);
    if (live.length > 0) {
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('On this computer'));
        for (const device of live) {
            const card = box({style_class: `desklink-card ${variant} desklink-live`});
            card.add_child(new St.Widget({style_class: 'desklink-dot coral', y_align: Clutter.ActorAlign.CENTER}));
            card.add_child(new St.Icon({
                icon_name: device.screen ? 'video-display-symbolic' : 'utilities-terminal-symbolic',
                style_class: 'desklink-card-icon',
            }));
            const words = column();
            words.add_child(label(device.name, 'desklink-card-title'));
            const detail = () => [liveWords(device), device.liveSince ? duration(device.liveSince) : '']
                .filter(Boolean).join(' · ');
            const since = dim(label(detail(), 'desklink-card-subtitle'));
            if (device.liveSince)
                ctx.tick(() => (since.text = detail()));
            words.add_child(since);
            card.add_child(words);
            const end = textButton('End', () => invoke(device.id, 'end-sessions'), 'desklink-end');
            end.accessible_name = `End what ${device.name} is doing here`;
            card.add_child(end);
            ctx.appear(card, `live:${device.id}`);
            menu.addMenuItem(row(card));
        }
        if (live.length > 1)
            menu.addAction('End Everything', () => invoke('', 'end-sessions'));
    }

    // Recent files, opened on click.
    if (transfers.length > 0) {
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Recent files'));
        for (const file of transfers.slice(0, 3)) {
            const item = new PopupMenu.PopupBaseMenuItem({style_class: 'desklink-row desklink-file'});
            item.add_child(new St.Icon({gicon: fileIcon(file), style_class: 'desklink-thumb'}));
            const words = column();
            words.add_child(label(file.name, 'desklink-file-name'));
            const when = file.time ? GLib.DateTime.new_from_unix_local(file.time).format('%H:%M') : '';
            words.add_child(dim(label(`${file.incoming ? 'from' : 'to'} ${file.deviceName}${when ? ` · ${when}` : ''}`, 'desklink-card-subtitle')));
            item.add_child(words);
            item.accessible_name = `${file.name}, ${file.incoming ? 'from' : 'to'} ${file.deviceName}`;
            item.connect('activate', () => ctx.call('OpenTransfer', [file.id]));
            menu.addMenuItem(item);
        }
    }

    // Snippets: copied on click (the shell owns the clipboard).
    const snippets = ctx.snippets ?? [];
    if (snippets.length > 0) {
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Snippets'));
        for (const snippet of snippets.slice(0, 5)) {
            const item = new PopupMenu.PopupImageMenuItem(snippet.label, 'edit-copy-symbolic');
            item.accessible_name = `Copy ${snippet.label}`;
            item.connect('activate', () =>
                St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, snippet.text));
            menu.addMenuItem(item);
        }
    }

    menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
    menu.addAction('New Note', () => invoke('', 'new-note'), 'document-edit-symbolic');
    menu.addAction('Open DeskLink', () => invoke(selected.id, 'open'));
}
