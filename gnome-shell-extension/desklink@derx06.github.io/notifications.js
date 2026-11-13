// SPDX-License-Identifier: GPL-2.0-or-later
//
// The phones' notifications as GNOME's own: one message-tray source per
// phone app ("WhatsApp · realme 14 5G"), the phone's actions as buttons, and
// Reply in a small dialog. A notification resent on a reconnect (`silent`),
// or present when the extension starts, goes to the list without a banner.
// Dismissing one here dismisses it on the phone; one the phone removes goes
// from here. Everything goes through DeskLink's own D-Bus interface.

import Clutter from 'gi://Clutter';
import St from 'gi://St';
import GLib from 'gi://GLib';

import * as Config from 'resource:///org/gnome/shell/misc/config.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as MessageTray from 'resource:///org/gnome/shell/ui/messageTray.js';
import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';

// GNOME 46 moved sources and notifications to property-based constructors.
const SHELL_MAJOR = parseInt(Config.PACKAGE_VERSION.split('.')[0]);

// BEGIN notify plan
// What to show, replace or take back for the phones' notifications: pure, so
// it is tested in node (tests/notifications.test.mjs). `trace(line)` gets one
// line per decision (key, verdict, reason; never the text).
function key(...parts) {
    return parts.join('\u0000');
}

// Its words: a change here is news (replaced, and maybe a banner).
function contentOf(entry) {
    return JSON.stringify([entry.app, entry.title, entry.body]);
}

// Its words without digits ("Downloading 41%", "3 new messages").
function shapeOf(entry) {
    return contentOf(entry).replace(/[0-9]/g, '');
}

// Its buttons and whether the phone is reachable: a change here redraws the
// notification where it is, never a banner (a dropped link and its return
// used to banner every notification twice).
function layoutOf(entry) {
    return JSON.stringify([entry.actions, entry.replyable, entry.ready]);
}

const BANNER_GAP_MS = 4000;
const BANNERS_PER_10S = 5;
const REPOST_QUIET_MS = 10000;
const COUNTER_GAP_MS = 60000;
const TWIN_MS = 5000;

class NotifyPlan {
    constructor(trace = () => {}) {
        this._trace = trace;
        // id -> {content, layout, shape}
        this._shown = new Map();
        // Closed here (dismissed, or acted on) while the phone still lists
        // it: not shown again unless its words change.
        this._closed = new Map();
        // Taken back because the phone removed it: id -> {content, at}.
        this._gone = new Map();
        // id -> when it last raised a banner (and its shape then).
        this._banners = new Map();
        this._recent = [];
        // device + words -> when announced.
        this._twins = new Map();
    }

    /// `entries` (the `Notifications` property) -> the operations to run, in
    /// order: {op: 'show', id, entry, quiet, replace} / {op: 'withdraw', id}.
    /// `quiet`: nothing new banners (the first read after starting or reconnecting).
    sync(entries, quiet, now) {
        const ops = [];
        const wanted = new Set();
        for (const entry of entries) {
            const id = key(entry.device, entry.id);
            wanted.add(id);
            const content = contentOf(entry);
            const layout = layoutOf(entry);
            const known = this._shown.get(id);
            if (known && known.content === content) {
                if (known.layout === layout)
                    continue;
                // Only its buttons or the link changed: redrawn quietly.
                known.layout = layout;
                this._trace(`${id}: replaced quietly (buttons or link changed)`);
                ops.push({op: 'show', id, entry, quiet: true, replace: true});
                continue;
            }
            if (this._closed.get(id) === content) {
                this._trace(`${id}: skipped (closed here)`);
                continue;
            }
            this._closed.delete(id);
            let why = quiet ? 'first read' : entry.silent ? 'resync' : null;
            if (!why)
                why = this._banner(id, entry, content, known, now);
            this._trace(`${id}: ${known ? 'replaced' : 'added'} ${why ? `quietly (${why})` : 'with a banner'}`);
            this._shown.set(id, {content, layout, shape: shapeOf(entry)});
            ops.push({op: 'show', id, entry, quiet: why !== null, replace: !!known});
        }
        for (const [id, record] of [...this._shown]) {
            if (!wanted.has(id)) {
                this._shown.delete(id);
                this._gone.set(id, {content: record.content, at: now});
                this._trace(`${id}: withdrawn (the phone removed it)`);
                ops.push({op: 'withdraw', id});
            }
        }
        for (const id of [...this._closed.keys()]) {
            if (!wanted.has(id))
                this._closed.delete(id);
        }
        for (const [id, record] of [...this._gone]) {
            if (now - record.at >= REPOST_QUIET_MS)
                this._gone.delete(id);
        }
        for (const [id, at] of [...this._twins]) {
            if (now - at >= TWIN_MS)
                this._twins.delete(id);
        }
        return ops;
    }

    /// Why `id` may not raise a banner now, or null when it may.
    _banner(id, entry, content, known, now) {
        const gone = this._gone.get(id);
        if (gone && gone.content === content && now - gone.at < REPOST_QUIET_MS)
            return 're-post of a removed one';
        const twin = key(entry.device, content);
        if (this._twins.has(twin))
            return 'same words under another id';
        const last = this._banners.get(id);
        if (known && last && last.shape === shapeOf(entry) && now - last.at < COUNTER_GAP_MS)
            return 'a counter ticking';
        this._recent = this._recent.filter(at => now - at < 10000);
        if ((last && now - last.at < BANNER_GAP_MS) || this._recent.length >= BANNERS_PER_10S)
            return 'too soon after another banner';
        this._banners.set(id, {at: now, shape: shapeOf(entry)});
        this._recent.push(now);
        this._twins.set(twin, now);
        return null;
    }

    /// The person closed (or acted on) the notification shown for `id`.
    closedHere(id) {
        const record = this._shown.get(id);
        if (!record)
            return;
        this._shown.delete(id);
        this._closed.set(id, record.content);
        this._trace(`${id}: closed here`);
    }

    /// Everything is going away (the extension is disabled).
    reset() {
        this._shown.clear();
    }

    ids() {
        return [...this._shown.keys()];
    }
}
// END notify plan

export class PhoneNotifications {
    constructor(call) {
        // call(method, args): a method on DeskLink's Shell1 interface.
        this._call = call;
        this._sources = new Map();
        // id -> {notification, withdrawn, acted}: what is on screen now.
        this._records = new Map();
        // DESKLINK_DEBUG=1 in the shell's environment, or the file
        // ~/.cache/desklink/debug-notifications: one journal line per decision.
        const debug = GLib.getenv('DESKLINK_DEBUG') === '1' ||
            GLib.file_test(GLib.build_filenamev([GLib.get_user_cache_dir(), 'desklink', 'debug-notifications']),
                GLib.FileTest.EXISTS);
        this._plan = new NotifyPlan(debug ? line => console.log(`[DeskLink notifications] ${line}`) : () => {});
        // Reply dialogs open now (destroyed when closed, and on disable).
        this._dialogs = new Set();
    }

    /// Shows `entries` (the `Notifications` property). `quiet`: nothing new
    /// banners (the first read after starting or reconnecting).
    sync(entries, quiet) {
        const now = GLib.get_monotonic_time() / 1000;
        for (const op of this._plan.sync(entries, quiet, now)) {
            if (op.op === 'withdraw') {
                this._withdraw(op.id);
            } else {
                if (op.replace)
                    this._withdraw(op.id);
                this._show(op.id, op.entry, op.quiet);
            }
        }
    }

    destroy() {
        for (const dialog of [...this._dialogs])
            dialog.destroy();
        this._dialogs.clear();
        for (const id of [...this._records.keys()])
            this._withdraw(id);
        this._plan.reset();
        for (const source of this._sources.values())
            source.destroy();
        this._sources.clear();
    }

    _source(entry) {
        const id = key(entry.device, entry.app);
        let source = this._sources.get(id);
        if (source)
            return source;
        const title = `${entry.app} · ${entry.deviceName}`;
        source = SHELL_MAJOR >= 46
            ? new MessageTray.Source({title, iconName: entry.icon})
            : new MessageTray.Source(title, entry.icon);
        source.connect('destroy', () => this._sources.delete(id));
        Main.messageTray.add(source);
        this._sources.set(id, source);
        return source;
    }

    _show(id, entry, quiet) {
        const source = this._source(entry);
        const urgency = quiet ? MessageTray.Urgency.LOW : MessageTray.Urgency.NORMAL;
        let notification;
        if (SHELL_MAJOR >= 46) {
            notification = new MessageTray.Notification({
                source,
                title: entry.title || entry.app,
                body: entry.body,
                urgency,
            });
        } else {
            notification = new MessageTray.Notification(source, entry.title || entry.app, entry.body);
            notification.setUrgency(urgency);
        }
        // Already seen: in the list, never a banner.
        notification.acknowledged = quiet;
        const record = {notification, withdrawn: false, acted: false};
        // Stale (the phone is not connected): nothing to act on. An action
        // closes the notification here; that is not a dismissal.
        if (entry.ready) {
            for (const action of entry.actions) {
                notification.addAction(action, () => {
                    record.acted = true;
                    this._call('NotificationAction', [entry.device, entry.id, action]);
                });
            }
            if (entry.replyable) {
                notification.addAction('Reply', () => {
                    record.acted = true;
                    this._reply(entry);
                });
            }
        }
        notification.connect('destroy', (_notification, reason) => {
            if (this._records.get(id) === record)
                this._records.delete(id);
            if (!record.withdrawn)
                this._plan.closedHere(id);
            // Closed here by the user: dismiss it on the phone too.
            if (!record.withdrawn && !record.acted && entry.ready && entry.dismissable &&
                reason === MessageTray.NotificationDestroyedReason.DISMISSED)
                this._call('NotificationDismiss', [entry.device, entry.id]);
        });
        this._records.set(id, record);
        if (SHELL_MAJOR >= 46)
            source.addNotification(notification);
        else
            source.showNotification(notification);
    }

    /// Removes a notification the phone no longer has (or replaced).
    _withdraw(id) {
        const record = this._records.get(id);
        if (!record)
            return;
        record.withdrawn = true;
        this._records.delete(id);
        record.notification.destroy(MessageTray.NotificationDestroyedReason.SOURCE_CLOSED);
    }

    _reply(entry) {
        const dialog = new ModalDialog.ModalDialog({styleClass: 'desklink-reply-dialog'});
        this._dialogs.add(dialog);
        dialog.connect('closed', () => {
            this._dialogs.delete(dialog);
            dialog.destroy();
        });
        const heading = new St.Label({
            text: `Reply to ${entry.title || entry.app}`,
            style_class: 'desklink-reply-heading',
        });
        const quote = new St.Label({text: entry.body, style_class: 'desklink-reply-quote'});
        quote.clutter_text.line_wrap = true;
        const field = new St.Entry({
            hint_text: `Message on ${entry.deviceName}`,
            can_focus: true,
            x_expand: true,
            style_class: 'desklink-reply-entry',
        });
        field.clutter_text.set_activatable?.(true);
        dialog.contentLayout.add_child(heading);
        dialog.contentLayout.add_child(quote);
        dialog.contentLayout.add_child(field);
        const send = () => {
            const text = field.get_text().trim();
            if (text)
                this._call('NotificationReply', [entry.device, entry.id, text]);
            dialog.close();
        };
        field.clutter_text.connect('activate', send);
        dialog.setButtons([
            {label: 'Cancel', action: () => dialog.close(), key: Clutter.KEY_Escape},
            {label: 'Send', action: send, default: true},
        ]);
        dialog.setInitialKeyFocus(field);
        dialog.open();
    }
}

/// An incoming call ringing on a phone, as a critical banner with Answer and
/// Decline (DeskLink's `call-answer` / `call-decline` actions), taken back as
/// soon as it stops ringing. Shown only for a phone whose `call` is ringing
/// and incoming (Shell1 version 4).
export class PhoneCalls {
    constructor(call) {
        this._call = call;
        this._source = null;
        // "device\0callId" → its notification.
        this._shown = new Map();
        // Answered, declined or closed here: not shown again for that call.
        this._closed = new Set();
    }

    sync(devices) {
        const ringing = new Map();
        for (const device of devices) {
            const call = device.ready ? device.call : null;
            if (call?.state === 'ringing' && call.incoming)
                ringing.set(key(device.id, call.id), {device, call});
        }
        for (const [id, record] of [...this._shown]) {
            if (!ringing.has(id)) {
                this._shown.delete(id);
                record.withdrawn = true;
                record.notification.destroy(MessageTray.NotificationDestroyedReason.SOURCE_CLOSED);
            }
        }
        for (const id of [...this._closed]) {
            if (!ringing.has(id))
                this._closed.delete(id);
        }
        for (const [id, {device, call}] of ringing) {
            if (!this._shown.has(id) && !this._closed.has(id))
                this._show(id, device, call);
        }
    }

    destroy() {
        this.sync([]);
        this._source?.destroy();
        this._source = null;
    }

    _ensureSource() {
        if (this._source)
            return this._source;
        const title = 'Phone calls';
        const iconName = 'call-start-symbolic';
        this._source = SHELL_MAJOR >= 46
            ? new MessageTray.Source({title, iconName})
            : new MessageTray.Source(title, iconName);
        this._source.connect('destroy', () => {
            this._source = null;
        });
        Main.messageTray.add(this._source);
        return this._source;
    }

    _show(id, device, call) {
        const source = this._ensureSource();
        const title = call.caller;
        let body = `Incoming call on ${device.name}`;
        if (call.number && call.number !== call.caller)
            body += ` · ${call.number}`;
        let notification;
        if (SHELL_MAJOR >= 46) {
            notification = new MessageTray.Notification({
                source, title, body, urgency: MessageTray.Urgency.CRITICAL,
            });
        } else {
            notification = new MessageTray.Notification(source, title, body);
            notification.setUrgency(MessageTray.Urgency.CRITICAL);
        }
        for (const [action, label] of [['call-answer', 'Answer'], ['call-decline', 'Decline']]) {
            if (device.actions.includes(action))
                notification.addAction(label, () => this._call('Invoke', [device.id, action]));
        }
        const record = {notification, withdrawn: false};
        notification.connect('destroy', () => {
            if (this._shown.get(id) === record)
                this._shown.delete(id);
            if (!record.withdrawn)
                this._closed.add(id);
        });
        this._shown.set(id, record);
        if (SHELL_MAJOR >= 46)
            source.addNotification(notification);
        else
            source.showNotification(notification);
    }
}
