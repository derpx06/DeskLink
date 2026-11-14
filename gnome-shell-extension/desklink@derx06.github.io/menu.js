// SPDX-License-Identifier: GPL-2.0-or-later
//
// The top-bar menu's content, drawn from model.js and kept small: the phone's
// name with its link and a battery ring, a phone switcher with two or more
// phones, six quick actions as pills, one row for the phone's music (its
// timeline on request), a row for each thing going on now (only while one
// is), every area folded into More, and a footer. Stock St/PopupMenu widgets
// and the shell's own button and quick-toggle styles, so it follows
// light/dark, the accent colour and high contrast; only the mint and coral
// status marks are DeskLink's own.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Slider} from 'resource:///org/gnome/shell/ui/slider.js';

import {
    footer, headerLine, liveWords, mediaClock, mediaPosition, menuBlocks, moreSummary, quickTiles, sections, seekReason,
    seekThrottle, statusLine, timer, duration,
} from './model.js';

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
            x_expand: false,
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

/// One pill of the action grid: a 16 px icon and a short label that is cut
/// short rather than widening the menu.
function tile(icon, text, {name = text, enabled = true, variant = '', onClick}) {
    const content = new St.BoxLayout({x_expand: true, x_align: Clutter.ActorAlign.CENTER, style_class: 'desklink-tile-content'});
    content.add_child(new St.Icon({icon_name: icon, style_class: 'desklink-tile-icon', y_align: Clutter.ActorAlign.CENTER}));
    content.add_child(label(text, 'desklink-tile-label'));
    const button = new St.Button({
        child: content,
        style_class: `button desklink-tile ${variant}`,
        can_focus: enabled,
        reactive: enabled,
        x_expand: true,
        accessible_name: name,
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


/// A one-line progress row: icon, title, the detail at the end and a thin bar
/// under them. `update(title, detail, fraction)` moves it in place.
function progressItem(icon, activatable) {
    const item = new PopupMenu.PopupBaseMenuItem({
        style_class: 'desklink-row desklink-progress-row', activate: activatable, hover: activatable, can_focus: activatable,
    });
    const words = column({style_class: 'desklink-progress-words'});
    const head = box({style_class: 'desklink-line'});
    head.add_child(new St.Icon({icon_name: icon, style_class: 'desklink-row-icon', y_align: Clutter.ActorAlign.CENTER}));
    const line = label('', 'desklink-row-title', {x_expand: true});
    head.add_child(line);
    const detail = dim(label('', 'desklink-row-detail'));
    head.add_child(detail);
    words.add_child(head);
    // Drawn at paint time from its own size: a fill sized in JS lagged the
    // menu's allocation and showed as a dot.
    const track = new St.DrawingArea({style_class: 'desklink-progress', x_expand: true});
    let fraction = 0;
    track.connect('repaint', area => {
        const cr = area.get_context();
        const [width, height] = area.get_surface_size();
        const fg = area.get_theme_node().get_foreground_color(); // the accent (stylesheet)
        const bar = (w, alpha) => {
            const r = height / 2;
            cr.setSourceRGBA(fg.red / 255, fg.green / 255, fg.blue / 255, alpha);
            cr.arc(r, r, r, Math.PI / 2, 3 * Math.PI / 2);
            cr.arc(Math.max(r, w - r), r, r, -Math.PI / 2, Math.PI / 2);
            cr.closePath();
            cr.fill();
        };
        bar(width, 0.25);
        if (fraction > 0)
            bar(Math.max(height, width * fraction), 1);
        cr.$dispose();
    });
    words.add_child(track);
    item.add_child(words);
    const draw = () => track.queue_repaint();
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
            item.add_child(trailing(entry.note));
        if (entry.danger) {
            item.add_style_class_name('desklink-danger');
            item.add_style_class_name(`desklink-${styleVariant()}`);
        }
        if (!entry.enabled)
            item.setSensitive(false);
        item.connect('activate', () => entry.action === 'send-link' ? ctx.sendLink(id) : ctx.invoke(send, entry.action));
        return item;
    }
    case 'switch': {
        if (entry.sensitive && !entry.on) {
            // Off for a security reason: turned on only in DeskLink itself.
            const item = new PopupMenu.PopupImageMenuItem(entry.label, 'changes-prevent-symbolic');
            item.add_child(trailing('Off · in DeskLink'));
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
            item.add_child(dim(label(entry.note, 'desklink-row-detail')));
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
        const item = new PopupMenu.PopupBaseMenuItem({style_class: 'desklink-file'});
        item.add_child(new St.Icon({gicon: fileIcon(file), style_class: 'popup-menu-icon'}));
        item.add_child(label(file.name, 'desklink-file-name', {x_expand: true}));
        const when = file.time ? GLib.DateTime.new_from_unix_local(file.time).format('%H:%M') : '';
        item.add_child(dim(label(`${entry.detail}${when ? ` · ${when}` : ''}`, 'desklink-row-detail')));
        item.accessible_name = `${file.name}, ${entry.detail}`;
        item.connect('activate', () => ctx.call('OpenTransfer', [file.id]));
        return item;
    }
    }
}

/// Scrolls `scroll` so `actor` (and the rows under it) is in view.
function reveal(scroll, actor, extra = 0) {
    const adjustment = scroll.vadjustment ?? scroll.get_vadjustment?.() ?? scroll.vscroll?.adjustment;
    if (!adjustment)
        return;
    const [, top] = actor.get_transformed_position();
    const [, viewTop] = scroll.get_transformed_position();
    const [, height] = actor.get_preferred_height(-1);
    const from = adjustment.value + (top - viewTop);
    const to = from + height + extra; // the section's header and its rows
    if (to > adjustment.value + scroll.height)
        adjustment.value = Math.min(from, to - scroll.height);
    else if (from < adjustment.value)
        adjustment.value = from;
}

/// Runs `fn` after `ms` unless `actor` is gone by then (its source is removed
/// with it, so nothing outlives the menu).
function defer(actor, ms, fn) {
    let id = 0;
    id = GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
        id = 0;
        try {
            fn();
        } catch (error) {
            // Best effort: the menu still works without it.
        }
        return GLib.SOURCE_REMOVE;
    });
    actor.connect('destroy', () => {
        if (id)
            GLib.source_remove(id);
        id = 0;
    });
}

/// Scrolls the menu so an opened section's rows are on screen (a tall menu
/// scrolls inside its own scroller). The rows are laid out once the section
/// has opened, so it looks twice.
function keepInView(item) {
    const show = () => {
        let parent = item.get_parent();
        while (parent && !(parent instanceof St.ScrollView))
            parent = parent.get_parent();
        if (parent && item.mapped)
            reveal(parent, item, item.menu.actor.get_preferred_height(-1)[1]);
    };
    defer(item, 0, show);
    defer(item, 320, show);
}

/// The sections' own scroller: a tall menu is clipped by the screen, so the
/// list scrolls inside a height that leaves the footer on screen. Returns the
/// section that rows go into. Parented like any section (activating a row
/// closes the menu, keys move between rows).
function scrolledSection(menu) {
    const wrapper = new PopupMenu.PopupMenuSection();
    menu.addMenuItem(wrapper);
    const holder = new PopupMenu.PopupMenuSection();
    wrapper.addMenuItem(holder);
    wrapper.box.remove_child(holder.actor);
    const scroller = new St.ScrollView({
        style_class: 'desklink-scroller',
        hscrollbar_policy: St.PolicyType.NEVER,
        vscrollbar_policy: St.PolicyType.AUTOMATIC,
        overlay_scrollbars: true,
        x_expand: true,
        child: holder.actor,
    });
    wrapper.box.add_child(scroller);
    if (menu.insetForScrollbar)
        holder.actor.style = 'padding-right: 12px;';
    let alive = true;
    scroller.connect('destroy', () => (alive = false));
    const fit = () => {
        if (!alive || !scroller.mapped)
            return;
        try {
            const monitor = Main.layoutManager.findMonitorForActor?.(scroller) ?? Main.layoutManager.primaryMonitor;
            scroller.style = '';
            holder.actor.style = menu.insetForScrollbar ? 'padding-right: 12px;' : '';
            const [, natural] = scroller.get_preferred_height(-1);
            const [, boxHeight] = menu.box.get_preferred_height(-1);
            const [, boxTop] = menu.box.get_transformed_position();
            const excess = boxTop + boxHeight - (monitor.y + monitor.height - 24);
            scroller.style = excess > 0 ? `max-height: ${Math.floor(Math.max(160, natural - excess))}px;` : '';
            // The pill's own popup scrolls by itself when it is tall: its scrollbar needs the room too.
            holder.actor.style = excess > 0 || menu.insetForScrollbar ? 'padding-right: 12px;' : '';
        } catch (error) {
            // Best effort: without it the menu is only as tall as it is.
        }
    };
    const later = () => defer(scroller, 0, fit);
    scroller.connect('notify::mapped', later);
    holder.fit = later;
    return holder;
}

/// The text at the end of a row ("3 new", "Off · in DeskLink").
function trailing(text) {
    return dim(label(text, 'desklink-row-detail', {x_expand: true, x_align: Clutter.ActorAlign.END}));
}

/// More: every area, folded into one row. Open, it lists the areas; picking
/// one shows its rows under a Back row, in place (no rebuild, so keyboard
/// focus moves on to the first row and Back returns it to the area). Which
/// area is shown survives a rebuild while the menu is open.
function addMore(menu, list, selected, ctx) {
    const more = new PopupMenu.PopupSubMenuMenuItem('More', true);
    more.add_style_class_name('desklink-more');
    more.icon.icon_name = 'view-more-symbolic';
    const summary = moreSummary(list);
    more.accessible_name = summary ? `More, ${summary}` : 'More';
    if (summary)
        more.insert_child_at_index(trailing(summary), Math.max(0, more.get_n_children() - 1));

    const entries = [];
    const views = new Map();
    let show = () => {};
    for (const section of list) {
        const entry = new PopupMenu.PopupImageMenuItem(section.title, section.icon);
        entry.add_style_class_name('desklink-area');
        if (section.subtitle)
            entry.add_child(trailing(section.subtitle));
        entry.add_child(new St.Icon({
            icon_name: 'go-next-symbolic', style_class: 'desklink-row-icon desklink-next',
            x_expand: !section.subtitle, x_align: Clutter.ActorAlign.END, y_align: Clutter.ActorAlign.CENTER,
        }));
        entry.accessible_name = section.subtitle ? `${section.title}, ${section.subtitle}` : section.title;
        entry.activate = () => show(section.id, true);
        ctx.track?.(entry, `area:${section.id}`);
        more.menu.addMenuItem(entry);
        entries.push(entry);

        const back = new PopupMenu.PopupImageMenuItem(section.title, 'go-previous-symbolic');
        back.add_style_class_name('desklink-back');
        back.accessible_name = `Back to all areas, from ${section.title}`;
        back.activate = () => show(null, true, section.id);
        ctx.track?.(back, `back:${section.id}`);
        const rows = [back];
        for (const row_ of section.rows) {
            const item = sectionRow(row_, selected, ctx);
            ctx.track?.(item, `${section.id}:${row_.id}`);
            rows.push(item);
        }
        rows.forEach(item => {
            item.visible = false;
            more.menu.addMenuItem(item);
        });
        views.set(section.id, rows);
    }

    show = (id, focus = false, from = null) => {
        const current = views.has(id) ? id : null;
        entries.forEach(entry => (entry.visible = current === null));
        for (const [key, rows] of views)
            rows.forEach(item => (item.visible = key === current));
        ctx.sectionShown?.(current);
        if (focus) {
            const target = current !== null ? views.get(current)[0] : entries[list.findIndex(s => s.id === from)] ?? entries[0];
            target?.grab_key_focus();
            menu.fit?.();
            keepInView(more);
        }
    };
    show(ctx.openSection);

    more.menu.connect('open-state-changed', (_menu, open) => {
        ctx.moreOpened?.(open);
        if (open) {
            // The shell gives a submenu its own scrollbar when the menu is
            // as tall as the screen; this menu scrolls as a whole instead.
            defer(more, 0, () => {
                more.menu.actor.vscrollbar_policy = St.PolicyType.NEVER;
                more.menu.actor.remove_style_pseudo_class('scrolled');
                menu.fit?.();
            });
            keepInView(more);
        } else {
            menu.fit?.();
        }
    });
    ctx.track?.(more, 'more');
    menu.addMenuItem(more);
    if (ctx.moreOpen)
        more.menu.open(false);
}

/// The track's timeline: elapsed, a slider, the length. Dragging or clicking
/// seeks the phone's player (at most five seeks a second, the last value
/// always); a seek holds the bar for a moment so the old position still in
/// flight does not pull it back. A stream or a player that cannot seek keeps
/// the bar, dimmed, with the reason under it.
function seekRow(selected, media, reason, ctx) {
    const length = media.lengthMs;
    const wrap = column({style_class: 'desklink-seek'});
    const line = box({style_class: 'desklink-seek-line'});
    const elapsed = dim(label('0:00', 'desklink-seek-time'));
    const total = dim(label(length > 0 ? mediaClock(length) : '–:––', 'desklink-seek-time', {x_align: Clutter.ActorAlign.END}));
    const slider = new Slider(0);
    slider.x_expand = true;
    slider.y_align = Clutter.ActorAlign.CENTER;
    line.add_child(elapsed);
    line.add_child(slider);
    line.add_child(total);
    wrap.add_child(line);

    const usable = reason === '';
    let base = {media, at: ctx.mediaAt ?? Date.now()};
    let held = null, dragging = false, updating = false;
    const named = position => {
        slider.accessible_name = usable
            ? `Track position, ${mediaClock(position)} of ${mediaClock(length)}`
            : `Track position, unavailable. ${reason}`;
    };
    const show = position => {
        updating = true;
        slider.value = length > 0 ? Math.min(1, position / length) : 0;
        elapsed.text = mediaClock(position);
        updating = false;
    };
    const refresh = () => {
        if (!dragging)
            show(mediaPosition(base.media, base.at, Date.now(), held));
    };
    refresh();
    named(mediaPosition(base.media, base.at, Date.now(), held));
    if (!usable) {
        slider.reactive = false;
        slider.can_focus = false;
        slider.opacity = 110;
        wrap.add_child(dim(label(reason || 'Can\'t seek', 'desklink-row-detail desklink-seek-reason')));
        ctx.tick(refresh);
        ctx.onMedia?.((now, at) => { base = {media: now, at}; refresh(); });
        return wrap;
    }
    const throttle = seekThrottle({
        send: ms => {
            held = {ms, until: Date.now() + 1500};
            ctx.seek(selected.id, ms);
        },
        now: () => Date.now(),
        schedule: (ms, fn) => GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => {
            fn();
            return GLib.SOURCE_REMOVE;
        }),
        cancel: id => GLib.source_remove(id),
    });
    slider.connect('notify::value', () => {
        if (updating)
            return;
        const ms = Math.round(slider.value * length);
        held = {ms, until: Date.now() + 1500};
        elapsed.text = mediaClock(ms);
        named(ms);
        throttle.change(ms);
    });
    slider.connect('drag-begin', () => (dragging = true));
    slider.connect('drag-end', () => {
        dragging = false;
        throttle.flush();
    });
    let seconds = 0;
    ctx.tick(() => {
        refresh();
        if (++seconds % 10 === 0 && !dragging)
            named(mediaPosition(base.media, base.at, Date.now(), held));
    });
    ctx.onMedia?.((now, at) => { base = {media: now, at}; refresh(); });
    return wrap;
}

/// The phone's music in one row: art, title and artist, previous, play/pause
/// and next. The title opens the timeline under it (when the app has one),
/// and it stays open across rebuilds while the menu is.
function mediaCard(selected, media, ctx, variant) {
    const can = action => selected.actions.includes(action);
    const card = column({style_class: 'desklink-media-card'});
    const line = box({style_class: 'desklink-media'});
    card.add_child(line);
    line.add_child(new St.Icon({icon_name: 'audio-x-generic-symbolic', style_class: `desklink-art ${variant}`, y_align: Clutter.ActorAlign.CENTER}));
    const words = column({style_class: 'desklink-media-words'});
    words.add_child(label(media.title, 'desklink-row-title'));
    if (media.artist)
        words.add_child(dim(label(media.artist, 'desklink-row-detail')));
    const reason = seekReason(selected, media);
    const track = media.artist ? `${media.title}, ${media.artist}` : media.title;
    if (reason === null) {
        line.add_child(words);
    } else {
        const head = box();
        head.add_child(words);
        const arrow = new St.Icon({style_class: 'desklink-row-icon desklink-chevron', y_align: Clutter.ActorAlign.CENTER});
        head.add_child(arrow);
        const opener = new St.Button({
            style_class: `desklink-media-opener ${variant}`, child: head, x_expand: true, can_focus: true,
            toggle_mode: true, checked: !!ctx.mediaOpen,
        });
        const seek = seekRow(selected, media, reason, ctx);
        const shown = open => {
            seek.visible = open;
            arrow.icon_name = open ? 'pan-up-symbolic' : 'pan-down-symbolic';
            opener.accessible_name = `${track}. ${open ? 'Hide' : 'Show'} the timeline`;
        };
        shown(!!ctx.mediaOpen);
        opener.connect('clicked', () => {
            shown(opener.checked);
            ctx.mediaOpened?.(opener.checked);
        });
        line.add_child(opener);
        card.add_child(seek);
    }
    if (can('media-prev'))
        line.add_child(iconButton('media-skip-backward-symbolic', 'Previous track', () => ctx.invoke(selected.id, 'media-prev'), variant));
    line.add_child(iconButton(media.playing ? 'media-playback-pause-symbolic' : 'media-playback-start-symbolic',
        media.playing ? 'Pause' : 'Play', () => ctx.invoke(selected.id, 'media-play-pause'), variant));
    line.add_child(iconButton('media-skip-forward-symbolic', 'Next track', () => ctx.invoke(selected.id, 'media-next'), variant));
    return card;
}

/// Builds the menu, block by block as `menuBlocks` lists them. `ctx`:
/// devices, selected, desk, transfers, snippets, activities (this phone's
/// ongoing items), activityText(item), linkInClipboard, openSection (the area
/// shown in More), sectionShown(id), moreOpen, moreOpened(open), mediaOpen,
/// mediaOpened(open), invoke(id, action), call(method, args),
/// setSwitch(id, key, on), sendLink(id), seek(id, ms), mediaAt (when the media
/// position was reported), onMedia(fn(media, at)) (the position moves in
/// place, not by a rebuild), select(id), appear(actor, key), tick(fn) (run
/// every second while open), track(item, key), ring (the header's battery).
export function fillMenu(menu, ctx) {
    const {devices, selected, desk, transfers, invoke} = ctx;
    const variant = `desklink-${styleVariant()}`;
    const can = (device, action) => device?.actions.includes(action);
    const activities = (ctx.activities ?? []).filter(item => item.device === selected?.id);
    if (menu.setHeader) {
        menu.setHeader('phone-symbolic', selected?.name ?? 'DeskLink', headerLine(selected));
        ctx.ring?.set(selected?.ready ? selected.battery : -1, !!selected?.charging);
    }
    menu.removeAll();
    if (!menu.setHeader && selected) {
        // A plain popup menu has no header: a title row in its place.
        const head = box({style_class: 'desklink-head'});
        const title = column();
        title.add_child(label(selected.name, 'desklink-head-title'));
        title.add_child(dim(label(headerLine(selected), 'desklink-row-detail')));
        head.add_child(title);
        const ring = new BatteryRing();
        ring.set(selected.ready ? selected.battery : -1, !!selected.charging);
        head.add_child(ring);
        menu.addMenuItem(row(head));
    }

    const blocks = menuBlocks(selected, devices, activities);
    if (blocks.includes('empty')) {
        menu.addMenuItem(row(dim(label('Pair a phone in DeskLink to see it here.', 'desklink-empty'))));
        menu.addAction('Open DeskLink', () => invoke('', 'open'));
        return;
    }

    // Everything between the phone's name and the footer scrolls when the
    // screen is too short for it.
    const body = scrolledSection(menu);
    for (const block of blocks) {
        switch (block) {
        case 'switcher': {
            // Phones: one chip each, the shown one checked.
            const chips = box({style_class: 'desklink-chips'});
            for (const device of devices) {
                const phoneChip = new St.Button({
                    style_class: `button desklink-chip ${variant}`,
                    toggle_mode: true,
                    checked: device.id === selected.id,
                    can_focus: true,
                    accessible_name: `${device.name}, ${statusLine(device)}`,
                });
                const content = box({style_class: 'desklink-chip-content'});
                content.add_child(new St.Widget({style_class: `desklink-dot ${device.ready ? 'mint' : ''}`, y_align: Clutter.ActorAlign.CENTER}));
                content.add_child(label(device.name, 'desklink-chip-label'));
                phoneChip.child = content;
                phoneChip.connect('clicked', () => ctx.select(device.id));
                chips.add_child(phoneChip);
            }
            body.addMenuItem(row(chips));
            break;
        }
        case 'offline': {
            // Not connected (the header says since when): a way to try again.
            const card = box({style_class: `desklink-card ${variant}`});
            card.add_child(new St.Icon({icon_name: 'network-offline-symbolic', style_class: 'desklink-row-icon', y_align: Clutter.ActorAlign.CENTER}));
            card.add_child(label(selected.reachable ? 'Connecting…' : 'Not connected', 'desklink-row-title', {x_expand: true}));
            card.add_child(can(selected, 'try-again')
                ? textButton('Try again', () => invoke(selected.id, 'try-again'))
                : textButton('Reconnect', () => invoke('', 'reconnect')));
            ctx.appear(card, `offline:${selected.id}`);
            body.addMenuItem(row(card));
            break;
        }
        case 'call': {
            // Answer/Decline while ringing, the time and End while on it.
            const call = selected.call;
            const ringing = call.state === 'ringing' && call.incoming;
            const card = box({style_class: `desklink-card ${variant} desklink-call ${ringing ? 'ringing' : ''}`});
            card.add_child(new St.Icon({icon_name: 'call-start-symbolic', style_class: 'desklink-call-icon', y_align: Clutter.ActorAlign.CENTER}));
            const words = column();
            words.add_child(label(call.caller, 'desklink-row-title'));
            const detail = dim(label('', 'desklink-row-detail'));
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
            body.addMenuItem(row(card));
            break;
        }
        case 'tiles': {
            // Six quick actions, three pills to a row, all the same width.
            const grid = column({style_class: 'desklink-grid'});
            const tiles = quickTiles(selected).map(entry => tile(entry.icon, entry.label, {
                variant, name: entry.name, enabled: entry.enabled,
                onClick: () => invoke(selected.id, entry.action),
            }));
            for (let i = 0; i < tiles.length; i += 3) {
                const line = box({style_class: 'desklink-grid-row'});
                line.layout_manager.homogeneous = true;
                tiles.slice(i, i + 3).forEach(t => line.add_child(t));
                grid.add_child(line);
            }
            body.addMenuItem(row(grid));
            break;
        }
        case 'media':
            body.addMenuItem(row(mediaCard(selected, selected.media, ctx, variant)));
            break;
        case 'transfers': {
            // Files moving: their progress moves every second, in place.
            const bars = selected.transfers.slice(0, 3).map((transfer, index) => {
                const bar = progressItem(transfer.incoming ? 'folder-download-symbolic' : 'folder-upload-symbolic', false);
                bar.show = now => bar.update(`${now.incoming ? 'Receiving' : 'Sending'} ${now.name}`,
                    [now.paused ? 'Paused' : (now.fraction > 0 ? `${Math.round(now.fraction * 100)}%` : 'Preparing'), now.lane]
                        .filter(Boolean).join(' · '), now.fraction);
                bar.show(transfer);
                ctx.appear(bar.item, `transfer:${selected.id}:${index}:${transfer.name}`);
                body.addMenuItem(bar.item);
                return bar;
            });
            ctx.onTransfers?.(list => bars.forEach((bar, index) => list[index] && bar.show(list[index])));
            break;
        }
        case 'bucket': {
            // The Files sync.
            const bucket = selected.bucket;
            const bar = progressItem('folder-symbolic', true);
            bar.update(bucket.line, bucket.detail, bucket.fraction);
            ctx.onBucket?.(now => now && bar.update(now.line, now.detail, now.fraction));
            bar.item.connect('activate', () => invoke(selected.id, 'open-bucket'));
            ctx.appear(bar.item, `bucket:${selected.id}`);
            body.addMenuItem(bar.item);
            break;
        }
        case 'activities':
            // The phone's timers, downloads, navigation.
            for (const activity of activities.slice(0, 2)) {
                const line = box({style_class: 'desklink-line'});
                line.add_child(new St.Icon({icon_name: 'alarm-symbolic', style_class: 'desklink-row-icon', y_align: Clutter.ActorAlign.CENTER}));
                line.add_child(label(activity.title || activity.app, 'desklink-row-title', {x_expand: true}));
                const detail = dim(label(ctx.activityText(activity), 'desklink-row-detail'));
                ctx.tick(() => (detail.text = ctx.activityText(activity)));
                line.add_child(detail);
                body.addMenuItem(row(line));
            }
            break;
        case 'live': {
            // A phone using this computer, with End.
            const live = devices.filter(device => device.screen || device.terminals > 0);
            for (const device of live) {
                const card = box({style_class: `desklink-card ${variant} desklink-live`});
                card.add_child(new St.Widget({style_class: 'desklink-dot coral', y_align: Clutter.ActorAlign.CENTER}));
                card.add_child(new St.Icon({
                    icon_name: device.screen ? 'video-display-symbolic' : 'utilities-terminal-symbolic',
                    style_class: 'desklink-row-icon', y_align: Clutter.ActorAlign.CENTER,
                }));
                const words = column();
                words.add_child(label(device.name, 'desklink-row-title'));
                const detail = () => [liveWords(device), device.liveSince ? duration(device.liveSince) : '']
                    .filter(Boolean).join(' · ');
                const since = dim(label(detail(), 'desklink-row-detail'));
                if (device.liveSince)
                    ctx.tick(() => (since.text = detail()));
                words.add_child(since);
                card.add_child(words);
                const end = textButton('End', () => invoke(device.id, 'end-sessions'), 'desklink-end');
                end.accessible_name = `End what ${device.name} is doing here`;
                card.add_child(end);
                ctx.appear(card, `live:${device.id}`);
                body.addMenuItem(row(card));
            }
            if (live.length > 1)
                body.addAction('End Everything', () => invoke('', 'end-sessions'));
            break;
        }
        case 'more':
            addMore(body, sections(selected, desk, {
                transfers: transfers.filter(file => file.deviceName === selected.name),
                snippets: ctx.snippets ?? [],
                linkInClipboard: !!ctx.linkInClipboard,
            }), selected, ctx);
            break;
        case 'footer': {
            // The app, its settings, and a pause for notifications.
            menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
            const bar = box({style_class: 'desklink-footer'});
            for (const button of footer(selected, desk)) {
                const view = new St.Button({
                    style_class: `button desklink-footer-button ${variant}`,
                    child: box({style_class: 'desklink-footer-content', x_align: Clutter.ActorAlign.CENTER}),
                    can_focus: true,
                    x_expand: true,
                    accessible_name: button.label,
                });
                view.child.add_child(new St.Icon({icon_name: button.icon, style_class: 'desklink-row-icon', y_align: Clutter.ActorAlign.CENTER}));
                view.child.add_child(label(button.short, 'desklink-footer-label'));
                view.connect('clicked', () => button.key
                    ? ctx.setSwitch(selected.id, button.key, button.on)
                    : invoke(selected.id, button.action));
                bar.add_child(view);
            }
            menu.addMenuItem(row(bar));
            break;
        }
        }
    }
}
