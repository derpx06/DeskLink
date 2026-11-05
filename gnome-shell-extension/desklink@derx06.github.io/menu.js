// SPDX-License-Identifier: GPL-2.0-or-later
//
// The top-bar menu's content, drawn from model.js: a header with the phone and
// a battery ring, status chips, the phone switcher, a call or offline card, six
// quick actions, the phone's music, what is going on now, one collapsed
// section per area (every action and setting of a paired phone), and a
// footer. Stock St/PopupMenu widgets and the shell's own button and
// quick-toggle styles, so it follows light/dark, the accent colour and high
// contrast; only the mint and coral status marks are DeskLink's own.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import {deviceChips, footer, liveWords, quickTiles, sections, statusLine, timer, duration, lastSeen} from './model.js';

export {lastSeen, sessionLine, shortStatus, statusLine} from './model.js';

const MINT = [0x2f / 255, 0xbf / 255, 0x8f / 255];
const CORAL = [0xe5 / 255, 0x53 / 255, 0x3d / 255];

/// The shell's light or dark style, for the card surfaces.
export function styleVariant() {
    return Main.getStyleVariant?.() === 'light' ? 'light' : 'dark';
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

/// A file's icon: its type's symbolic icon, never the file itself. A file
/// from a phone is not trusted to be decoded inside the shell (a decoder
/// crash would end the whole session), and a stat of its path on a hung
/// network folder would freeze the desktop.
function fileIcon(file) {
    try {
        const [type] = Gio.content_type_guess(file.name, null);
        return Gio.content_type_get_symbolic_icon(type);
    } catch (error) {
        return new Gio.ThemedIcon({name: 'text-x-generic-symbolic'});
    }
}


/// A status chip: a small rounded label, mint / coral / neutral / dimmed.
function chip(entry, variant) {
    return new St.Label({
        text: entry.text,
        style_class: `desklink-status-chip ${entry.tone} ${variant}`,
        y_align: Clutter.ActorAlign.CENTER,
        accessible_name: entry.text,
    });
}

/// Chips in lines of about 36 characters: a St box does not wrap.
function chipLines(chips, variant) {
    const lines = [[]];
    let width = 0;
    for (const entry of chips) {
        const need = entry.text.length + 4;
        if (width + need > 36 && lines.at(-1).length > 0) {
            lines.push([]);
            width = 0;
        }
        lines.at(-1).push(entry);
        width += need;
    }
    const column_ = column({style_class: 'desklink-chip-lines'});
    for (const line of lines) {
        const box_ = box({style_class: 'desklink-chip-line'});
        line.forEach(entry => box_.add_child(chip(entry, variant)));
        column_.add_child(box_);
    }
    return column_;
}

/// A progress row: icon and title, a detail line and a bar. `update(line,
/// detail, fraction)` moves it in place.
function progressItem(icon, activatable) {
    const item = new PopupMenu.PopupBaseMenuItem({
        style_class: 'desklink-row desklink-bucket', activate: activatable, hover: activatable, can_focus: activatable,
    });
    const words = column({style_class: 'desklink-bucket-words'});
    const head = box();
    head.add_child(new St.Icon({icon_name: icon, style_class: 'desklink-bucket-icon', y_align: Clutter.ActorAlign.CENTER}));
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
    const update = (title, text, value) => {
        line.text = title;
        detail.text = text;
        fraction = Math.min(1, Math.max(0, value ?? 0));
        item.accessible_name = `${title}, ${text}, ${Math.round(fraction * 100)}%`;
        draw();
    };
    return {item, update};
}

/// A switch row that keeps the menu open (the stock one closes it on a click).
function switchItem(text, on, onToggle) {
    const item = new PopupMenu.PopupSwitchMenuItem(text, !!on);
    item.activate = () => item.toggle();
    item.accessible_name = text;
    item.connect('toggled', (_item, state) => onToggle(state));
    return item;
}

/// One row of a section.
function sectionRow(entry, selected, ctx) {
    const id = selected.id;
    const send = entry.device === false ? '' : id;
    switch (entry.kind) {
    case 'action': {
        const item = new PopupMenu.PopupImageMenuItem(entry.label, entry.icon);
        item.accessible_name = entry.note ? `${entry.label}, ${entry.note}` : entry.label;
        if (entry.note)
            item.add_child(dim(label(entry.note, 'desklink-card-subtitle', {x_expand: true, x_align: Clutter.ActorAlign.END})));
        if (entry.danger)
            item.add_style_class_name('desklink-danger');
        if (!entry.enabled)
            item.setSensitive(false);
        item.connect('activate', () => entry.action === 'send-link' ? ctx.sendLink(id) : ctx.invoke(send, entry.action));
        return item;
    }
    case 'switch': {
        if (entry.sensitive && !entry.on) {
            // Off for a security reason: turned on only in DeskLink itself.
            const item = new PopupMenu.PopupImageMenuItem(entry.label, 'changes-prevent-symbolic');
            item.add_child(dim(label('Off · set up in DeskLink', 'desklink-card-subtitle', {x_expand: true, x_align: Clutter.ActorAlign.END})));
            item.accessible_name = `${entry.label}, off. Opens DeskLink to turn it on.`;
            item.connect('activate', () => ctx.invoke(id, 'open-settings'));
            return item;
        }
        const item = switchItem(entry.label, entry.on, state =>
            ctx.setSwitch(entry.scope === 'desk' ? '' : id, entry.key, state));
        if (entry.sensitive)
            item.add_style_class_name('desklink-sensitive');
        if (entry.note) {
            item.accessible_name = `${entry.label}, ${entry.note}`;
            item.add_child(dim(label(entry.note, 'desklink-card-subtitle')));
        }
        if (!entry.enabled)
            item.setSensitive(false);
        return item;
    }
    case 'snippet': {
        const item = new PopupMenu.PopupImageMenuItem(entry.label, 'edit-copy-symbolic');
        item.accessible_name = `Copy ${entry.label}`;
        item.connect('activate', () =>
            St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, entry.text));
        return item;
    }
    default: { // 'file'
        const file = entry.file;
        const item = new PopupMenu.PopupBaseMenuItem({style_class: 'desklink-row desklink-file'});
        item.add_child(new St.Icon({gicon: fileIcon(file), style_class: 'desklink-thumb'}));
        const words = column();
        words.add_child(label(file.name, 'desklink-file-name'));
        const when = file.time ? GLib.DateTime.new_from_unix_local(file.time).format('%H:%M') : '';
        words.add_child(dim(label(`${entry.detail}${when ? ` · ${when}` : ''}`, 'desklink-card-subtitle')));
        item.add_child(words);
        item.accessible_name = `${file.name}, ${entry.detail}`;
        item.connect('activate', () => ctx.call('OpenTransfer', [file.id]));
        return item;
    }
    }
}

/// A collapsed section: icon, title and a short state; one open at a time
/// (the shell's accordion), and the open one survives a rebuild.
function addSection(menu, section, selected, ctx) {
    const sub = new PopupMenu.PopupSubMenuMenuItem(section.title, true);
    sub.add_style_class_name('desklink-section');
    sub.icon.icon_name = section.icon;
    sub.accessible_name = section.subtitle ? `${section.title}, ${section.subtitle}` : section.title;
    if (section.subtitle && sub.status) {
        sub.status.text = section.subtitle;
        sub.status.add_style_class_name('desklink-section-state');
    }
    for (const entry of section.rows) {
        const item = sectionRow(entry, selected, ctx);
        ctx.track?.(item, `${section.id}:${entry.id}`);
        sub.menu.addMenuItem(item);
    }
    sub.menu.connect('open-state-changed', (_menu, open) => ctx.sectionOpened(section.id, open));
    ctx.track?.(sub, `section:${section.id}`);
    menu.addMenuItem(sub);
    if (ctx.openSection === section.id)
        sub.menu.open(0);
}

/// Builds the menu. `ctx`: devices, selected, desk, transfers, snippets,
/// activities (this phone's ongoing items), activityText(item),
/// linkInClipboard, openSection, sectionOpened(id, open),
/// invoke(id, action), call(method, args), setSwitch(id, key, on),
/// sendLink(id), select(id), appear(actor, key), tick(fn) (run every second
/// while open), track(item, key).
export function fillMenu(menu, ctx) {
    const {devices, selected, desk, transfers, invoke} = ctx;
    const variant = `desklink-${styleVariant()}`;
    const can = (device, action) => device?.actions.includes(action);
    if (menu.setHeader) {
        menu.setHeader('phone-symbolic', selected?.name ?? 'DeskLink', statusLine(selected));
        ctx.ring?.set(selected?.ready ? selected.battery : -1, !!selected?.charging);
    }
    menu.removeAll();
    if (!menu.setHeader && selected) {
        // A plain popup menu has no header: a title row in its place.
        const title = column({style_class: 'desklink-head'});
        title.add_child(label(selected.name, 'desklink-card-title'));
        title.add_child(dim(label(statusLine(selected), 'desklink-card-subtitle')));
        menu.addMenuItem(row(title));
    }

    // Phones: one chip each, the shown one checked.
    if (devices.length > 1) {
        const chips = box({style_class: 'desklink-chips'});
        for (const device of devices) {
            const phoneChip = new St.Button({
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
            phoneChip.child = content;
            phoneChip.connect('clicked', () => ctx.select(device.id));
            chips.add_child(phoneChip);
        }
        menu.addMenuItem(row(chips));
    }

    if (!selected) {
        menu.addMenuItem(row(dim(label('Pair a phone in DeskLink to see it here.', 'desklink-empty'))));
        menu.addAction('Open DeskLink', () => invoke('', 'open'));
        return;
    }

    // How it is: connection, route, battery, lane, sound, Do Not Disturb.
    menu.addMenuItem(row(chipLines(deviceChips(selected), variant)));

    // Not connected: when it was last here, and a way to try again.
    if (!selected.ready) {
        const card = box({style_class: `desklink-card ${variant}`});
        card.add_child(new St.Icon({icon_name: 'network-offline-symbolic', style_class: 'desklink-card-icon'}));
        const words = column();
        words.add_child(label(selected.reachable ? 'Connecting…' : 'Not connected', 'desklink-card-title'));
        words.add_child(dim(label(lastSeen(selected) || 'Waiting for the phone', 'desklink-card-subtitle')));
        card.add_child(words);
        if (can(selected, 'try-again'))
            card.add_child(textButton('Try again', () => invoke(selected.id, 'try-again')));
        else
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

    // Six quick actions, three to a row.
    const grid = column({style_class: 'desklink-grid'});
    const tiles = quickTiles(selected).map(entry => tile(entry.icon, entry.label, {
        variant, name: entry.name, enabled: entry.enabled,
        onClick: () => invoke(selected.id, entry.action),
    }));
    for (let i = 0; i < tiles.length; i += 3) {
        const line = box({style_class: 'desklink-grid-row'});
        tiles.slice(i, i + 3).forEach(t => line.add_child(t));
        grid.add_child(line);
    }
    menu.addMenuItem(row(grid));

    // The phone's music.
    const media = selected.ready ? selected.media : null;
    if (media) {
        const line = box({style_class: 'desklink-media'});
        line.add_child(new St.Icon({icon_name: 'audio-x-generic-symbolic', style_class: `desklink-art ${variant}`, y_align: Clutter.ActorAlign.CENTER}));
        const words = column();
        words.add_child(label(media.title, 'desklink-card-title'));
        if (media.artist)
            words.add_child(dim(label(media.artist, 'desklink-card-subtitle')));
        line.add_child(words);
        if (can(selected, 'media-prev'))
            line.add_child(iconButton('media-skip-backward-symbolic', 'Previous track', () => invoke(selected.id, 'media-prev'), variant));
        line.add_child(iconButton(media.playing ? 'media-playback-pause-symbolic' : 'media-playback-start-symbolic',
            media.playing ? 'Pause' : 'Play', () => invoke(selected.id, 'media-play-pause'), variant));
        line.add_child(iconButton('media-skip-forward-symbolic', 'Next track', () => invoke(selected.id, 'media-next'), variant));
        menu.addMenuItem(row(line));
    }

    // Going on now: files moving, the Files sync, the phone's timers.
    const moving = selected.transfers ?? [];
    const activities = (ctx.activities ?? []).filter(item => item.device === selected.id).slice(0, 2);
    const bucket = selected.bucket;
    if (moving.length > 0 || bucket || activities.length > 0)
        menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Now'));
    const bars = moving.slice(0, 3).map((transfer, index) => {
        const bar = progressItem(transfer.incoming ? 'folder-download-symbolic' : 'folder-upload-symbolic', false);
        bar.show = now => bar.update(`${now.incoming ? 'Receiving' : 'Sending'} ${now.name}`,
            [now.paused ? 'Paused' : (now.fraction > 0 ? `${Math.round(now.fraction * 100)}%` : 'Preparing'), now.lane]
                .filter(Boolean).join(' · '), now.fraction);
        bar.show(transfer);
        ctx.appear(bar.item, `transfer:${selected.id}:${index}:${transfer.name}`);
        menu.addMenuItem(bar.item);
        return bar;
    });
    // Their progress moves every second: updated in place, not rebuilt.
    ctx.onTransfers?.(list => bars.forEach((bar, index) => list[index] && bar.show(list[index])));
    if (bucket) {
        const bar = progressItem('folder-symbolic', true);
        bar.update(bucket.line, bucket.detail, bucket.fraction);
        ctx.onBucket?.(now => now && bar.update(now.line, now.detail, now.fraction));
        bar.item.connect('activate', () => invoke(selected.id, 'open-bucket'));
        ctx.appear(bar.item, `bucket:${selected.id}`);
        menu.addMenuItem(bar.item);
    }
    for (const activity of activities) {
        const item = new PopupMenu.PopupBaseMenuItem({activate: false, hover: false, can_focus: false, style_class: 'desklink-row'});
        item.add_child(new St.Icon({icon_name: 'alarm-symbolic', style_class: 'desklink-bucket-icon', y_align: Clutter.ActorAlign.CENTER}));
        const words = column();
        words.add_child(label(activity.title || activity.app, 'desklink-card-title'));
        const detail = dim(label(ctx.activityText(activity), 'desklink-card-subtitle'));
        ctx.tick(() => (detail.text = ctx.activityText(activity)));
        words.add_child(detail);
        item.add_child(words);
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

    // Every area, collapsed.
    menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
    for (const section of sections(selected, desk, {
        transfers: transfers.filter(file => file.deviceName === selected.name),
        snippets: ctx.snippets ?? [],
        linkInClipboard: !!ctx.linkInClipboard,
    }))
        addSection(menu, section, selected, ctx);

    // The footer: the app, its settings, and a pause for notifications.
    menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
    const bar = box({style_class: 'desklink-footer'});
    for (const button of footer(selected, desk)) {
        const view = new St.Button({
            style_class: `button desklink-pill-button desklink-footer-button ${variant}`,
            child: box({style_class: 'desklink-footer-content', x_align: Clutter.ActorAlign.CENTER}),
            can_focus: true,
            x_expand: true,
            accessible_name: button.label,
        });
        view.child.add_child(new St.Icon({icon_name: button.icon, style_class: 'desklink-footer-icon', y_align: Clutter.ActorAlign.CENTER}));
        view.child.add_child(label(button.short, 'desklink-footer-label'));
        view.connect('clicked', () => button.key
            ? ctx.setSwitch(selected.id, button.key, button.on)
            : invoke(selected.id, button.action));
        bar.add_child(view);
    }
    menu.addMenuItem(row(bar));
}
