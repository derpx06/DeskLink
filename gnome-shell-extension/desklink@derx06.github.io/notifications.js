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

import * as Config from 'resource:///org/gnome/shell/misc/config.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as MessageTray from 'resource:///org/gnome/shell/ui/messageTray.js';
import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';

// GNOME 46 moved sources and notifications to property-based constructors.
const SHELL_MAJOR = parseInt(Config.PACKAGE_VERSION.split('.')[0]);

function key(...parts) {
    return parts.join('\u0000');
}

function signature(entry) {
    return JSON.stringify([entry.title, entry.body, entry.actions, entry.replyable, entry.ready]);
}

export class PhoneNotifications {
    constructor(call) {
        // call(method, args): a method on DeskLink's Shell1 interface.
        this._call = call;
        this._sources = new Map();
        this._shown = new Map();
        // Closed here (dismissed, or acted on) while the phone still lists
        // it: not shown again unless its content changes.
        this._closed = new Map();
    }

    /// Shows `entries` (the `Notifications` property). `quiet`: nothing new
    /// banners (the first read after starting or reconnecting).
    sync(entries, quiet) {
        const wanted = new Set();
        for (const entry of entries) {
            const id = key(entry.device, entry.id);
            wanted.add(id);
            const known = this._shown.get(id);
            if (known && known.signature === signature(entry))
                continue;
            if (this._closed.get(id) === signature(entry))
                continue;
            this._closed.delete(id);
            if (known)
                this._withdraw(id);
            this._show(id, entry, quiet || entry.silent || !!known);
        }
        for (const id of [...this._shown.keys()]) {
            if (!wanted.has(id))
                this._withdraw(id);
        }
        for (const id of [...this._closed.keys()]) {
            if (!wanted.has(id))
                this._closed.delete(id);
        }
    }

    destroy() {
        for (const id of [...this._shown.keys()])
            this._withdraw(id);
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
        const record = {notification, signature: signature(entry), withdrawn: false, acted: false};
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
            this._shown.delete(id);
            if (!record.withdrawn)
                this._closed.set(id, record.signature);
            // Closed here by the user: dismiss it on the phone too.
            if (!record.withdrawn && !record.acted && entry.ready && entry.dismissable &&
                reason === MessageTray.NotificationDestroyedReason.DISMISSED)
                this._call('NotificationDismiss', [entry.device, entry.id]);
        });
        this._shown.set(id, record);
        if (SHELL_MAJOR >= 46)
            source.addNotification(notification);
        else
            source.showNotification(notification);
    }

    /// Removes a notification the phone no longer has (or replaced).
    _withdraw(id) {
        const record = this._shown.get(id);
        if (!record)
            return;
        record.withdrawn = true;
        this._shown.delete(id);
        record.notification.destroy(MessageTray.NotificationDestroyedReason.SOURCE_CLOSED);
    }

    _reply(entry) {
        const dialog = new ModalDialog.ModalDialog({styleClass: 'desklink-reply-dialog'});
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
