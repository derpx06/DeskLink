// SPDX-License-Identifier: GPL-2.0-or-later
//
// What the top-bar menu says and offers, as plain data. No GNOME imports, so
// node can test it (tests/model.test.mjs); menu.js only draws what this
// returns. Every action and switch named here is one the app's
// `derx06.desklink.com.Shell1` interface offers (a test pins that).

export const ROUTE = {lan: 'Wi-Fi', direct: 'Internet', turn: 'Relay', relay: 'Relay'};

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const two = n => String(n).padStart(2, '0');

/// "last seen 14:02", or "last seen 3 Nov 14:02" from another day.
export function lastSeen(device, now = new Date()) {
    if (!device?.lastSeen)
        return '';
    const seen = new Date(device.lastSeen * 1000);
    const time = `${two(seen.getHours())}:${two(seen.getMinutes())}`;
    const today = seen.toDateString() === now.toDateString();
    return `last seen ${today ? time : `${seen.getDate()} ${MONTHS[seen.getMonth()]} ${time}`}`;
}

/// "Connected · Wi-Fi · 82% ⚡", "last seen 14:02", "Connecting…".
export function statusLine(device, now = new Date()) {
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
    return lastSeen(device, now) || 'Not connected';
}

/// The header's line under the phone's name: the link in a few words
/// ("Connected · Wi-Fi · 12 ms · charging"); the battery is the ring beside it.
export function headerLine(device, now = new Date()) {
    if (!device)
        return 'No phone connected';
    if (device.ready) {
        const parts = ['Connected'];
        if (ROUTE[device.path])
            parts.push(device.rttMs >= 0 ? `${ROUTE[device.path]} · ${device.rttMs} ms` : ROUTE[device.path]);
        if (device.charging && device.battery >= 0)
            parts.push('charging');
        return parts.join(' · ');
    }
    return device.reachable ? 'Connecting…' : (lastSeen(device, now) || 'Not connected');
}

/// "Wi-Fi · 82%": the toggle's short subtitle (its title is the phone).
export function shortStatus(device, now = new Date()) {
    if (!device?.ready)
        return statusLine(device, now);
    const parts = [];
    if (ROUTE[device.path])
        parts.push(ROUTE[device.path]);
    if (device.battery >= 0)
        parts.push(`${device.battery}%`);
    return parts.join(' · ') || 'Connected';
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

/// "Viewing · 2 terminals": a live card's short line.
export function liveWords(device) {
    const parts = [];
    if (device.screen === 'control')
        parts.push('Controlling');
    else if (device.screen === 'view')
        parts.push('Viewing');
    else if (device.screen === 'glide')
        parts.push('Mouse and keyboard here');
    if (device.terminals > 0)
        parts.push(device.terminals === 1 ? '1 terminal' : `${device.terminals} terminals`);
    return parts.join(' · ');
}

/// "just now", "12 min", "1 h 05 min" since `unixSeconds`.
export function duration(unixSeconds, nowMs = Date.now()) {
    const minutes = Math.max(0, Math.floor((nowMs / 1000 - unixSeconds) / 60));
    if (minutes < 1)
        return 'just now';
    if (minutes < 60)
        return `${minutes} min`;
    return `${Math.floor(minutes / 60)} h ${two(minutes % 60)} min`;
}

/// "1:05", "1:02:03" since `unixMillis`.
export function timer(unixMillis, nowMs = Date.now()) {
    const seconds = Math.max(0, Math.floor((nowMs - unixMillis) / 1000));
    const [h, m, s] = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60];
    return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${m}:${two(s)}`;
}

const has = (device, action) => !!device?.actions?.includes(action);

/// The quick-action tiles: these six, three to a row, greyed out when the
/// phone does not take one now (and not shown at all while it is away).
export function quickTiles(device) {
    const ready = !!device?.ready;
    const name = device?.name ?? 'the phone';
    return [
        {id: 'send-files', icon: 'document-send-symbolic', label: 'Files', name: `Send files to ${name}`},
        {id: 'send-clipboard', icon: 'edit-paste-symbolic', label: 'Clipboard', name: `Send the clipboard to ${name}`},
        {id: 'ring', icon: 'audio-volume-high-symbolic', label: 'Ring', name: `Ring ${name}`},
        {id: 'lock', icon: 'system-lock-screen-symbolic', label: 'Lock', name: `Lock ${name}`},
        {id: 'show-screen', icon: 'video-display-symbolic', label: 'Screen', name: `Show the screen of ${name}`},
        {id: 'insert-photo', icon: 'camera-photo-symbolic', label: 'Photo', name: `Take a photo with ${name} and put it on the clipboard`},
    ].map(tile => ({...tile, action: tile.id, enabled: ready && has(device, tile.id)}));
}

// Rows -----------------------------------------------------------------

const act = (id, label, icon, extra = {}) =>
    ({kind: 'action', id, label, icon, action: id, enabled: true, ...extra});

/// A switch row. `scope` 'phone' reads `device.settings`, 'desk' reads
/// `desk`. `sensitive` ones are off for a security reason: the app only lets
/// them go OFF from here, so an off one sends you to DeskLink.
function sw(key, label, on, extra = {}) {
    return {kind: 'switch', id: key, key, label, on: !!on, scope: 'phone', sensitive: false, enabled: true, ...extra};
}

const SENSITIVE = new Set(['terminalAccess', 'unlockNearby', 'galleryAccept', 'phoneKeepAwake', 'glide', 'deskMic']);

function phoneSwitch(device, key, label, extra = {}) {
    if (!device?.settings || !(key in device.settings))
        return null;
    return sw(key, label, device.settings[key], {sensitive: SENSITIVE.has(key), ...extra});
}

function deskSwitch(desk, key, label, extra = {}) {
    if (desk?.[key] === undefined)
        return null;
    return sw(key, label, desk[key], {scope: 'desk', sensitive: SENSITIVE.has(key), ...extra});
}

function soundSwitch(device, desk, key, label) {
    if (!device?.sound)
        return null;
    const usable = desk?.soundAvailable !== false && !!device.ready;
    return sw(key, label, device.sound[key], {
        enabled: usable || !!device.sound[key],
        note: desk?.soundAvailable === false ? 'Not available on this computer' : (device.ready ? '' : 'Needs a connection'),
    });
}

const present = rows => rows.filter(Boolean);

/// The expandable sections for `device`, each only if it has something to
/// offer. `extras`: {snippets, transfers (recent files), linkInClipboard}.
export function sections(device, desk, extras = {}) {
    if (!device)
        return [];
    const name = device.name;
    const out = [];
    const add = (id, icon, title, rows, subtitle = '') => {
        const kept = present(rows);
        if (kept.length > 0)
            out.push({id, icon, title, subtitle, rows: kept});
    };
    const recent = (extras.transfers ?? []).slice(0, 3).map(file => ({
        kind: 'file', id: `file:${file.id}`, label: file.name, file,
        detail: `${file.incoming ? 'from' : 'to'} ${file.deviceName}`,
    }));
    const linkRow = has(device, 'send-link') &&
        act('send-link', 'Send copied link', 'insert-link-symbolic', {
            enabled: !!extras.linkInClipboard,
            note: extras.linkInClipboard ? '' : 'Copy a link first',
        });
    add('send', 'document-send-symbolic', 'Send & share', [
        has(device, 'send-files') && act('send-files', 'Send files…', 'document-send-symbolic'),
        has(device, 'send-clipboard') && act('send-clipboard', 'Send clipboard', 'edit-paste-symbolic'),
        linkRow,
        has(device, 'ping') && act('ping', 'Ping', 'network-transmit-receive-symbolic'),
        ...recent,
    ]);
    add('insert', 'camera-photo-symbolic', 'Insert from phone', [
        has(device, 'insert-photo') && act('insert-photo', 'Take a photo', 'camera-photo-symbolic'),
        has(device, 'insert-photo-file') && act('insert-photo-file', 'Take a photo as a file', 'document-save-symbolic'),
        has(device, 'insert-scan') && act('insert-scan', 'Scan a document', 'scanner-symbolic'),
        has(device, 'insert-text') && act('insert-text', 'Copy text from a picture', 'format-text-plain-symbolic'),
        has(device, 'insert-qr') && act('insert-qr', 'Scan a QR code', 'view-grid-symbolic'),
    ]);
    add('sound', 'audio-speakers-symbolic', 'Sound', [
        soundSwitch(device, desk, 'deskSound', 'Play this computer\'s sound there'),
        soundSwitch(device, desk, 'phoneMic', 'Use the phone as microphone'),
        soundSwitch(device, desk, 'phoneSound', 'Play the phone\'s sound here'),
    ], (device.sound && (device.sound.deskSound || device.sound.phoneMic || device.sound.phoneSound)) ? 'On' : '');
    add('notes', 'document-edit-symbolic', 'Notes & clipboard', [
        act('new-note', 'New note', 'document-edit-symbolic', {device: false}),
        ...(extras.snippets ?? []).slice(0, 5).map(snippet => ({
            kind: 'snippet', id: `snippet:${snippet.id}`, label: snippet.label, text: snippet.text,
        })),
        has(device, 'open-notes') && act('open-notes', 'Open notes', 'x-office-document-symbolic'),
        has(device, 'open-clipboard') && act('open-clipboard', 'Clipboard history', 'edit-paste-symbolic'),
        deskSwitch(desk, 'clipboard', 'Share the clipboard with phones'),
    ]);
    const unread = device.unread > 0 ? `${device.unread} new` : '';
    add('notifications', 'preferences-system-notifications-symbolic', 'Notifications', [
        phoneSwitch(device, 'showNotifications', 'Show its notifications here'),
        phoneSwitch(device, 'dndShare', 'Share Do Not Disturb', {
            enabled: desk?.dndSupported !== false || !!device.settings?.dndShare,
            note: desk?.dndSupported === false ? 'Not available on this desktop' : '',
        }),
        phoneSwitch(device, 'commandAlerts', 'Tell it when a command finishes'),
        deskSwitch(desk, 'pauseMediaOnCall', 'Pause music during calls'),
        deskSwitch(desk, 'forwardNotifications', 'Send this computer\'s notifications'),
        has(device, 'open-notifications') &&
            act('open-notifications', unread ? `Open notifications (${unread})` : 'Open notifications', 'mail-unread-symbolic'),
    ], unread);
    add('calls', 'call-start-symbolic', 'Calls & contacts', [
        phoneSwitch(device, 'namesCallers', 'Name callers from its contacts'),
        has(device, 'open-contacts') && act('open-contacts', 'Contacts…', 'x-office-address-book-symbolic'),
    ]);
    add('terminals', 'utilities-terminal-symbolic', 'Terminals', [
        has(device, 'new-terminal') && act('new-terminal', 'New shared terminal', 'list-add-symbolic'),
        has(device, 'open-terminals') && act('open-terminals', 'Open terminals', 'utilities-terminal-symbolic'),
        phoneSwitch(device, 'terminalAccess', 'Terminal access'),
    ], device.terminals > 0 ? `${device.terminals} attached` : '');
    add('screen', 'video-display-symbolic', 'Screen & control', [
        has(device, 'show-screen') && act('show-screen', 'Show its screen…', 'video-display-symbolic'),
        has(device, 'end-sessions') && act('end-sessions', 'End what it is doing here', 'process-stop-symbolic'),
        phoneSwitch(device, 'glide', 'Glide the mouse onto it'),
    ]);
    add('files', 'folder-symbolic', 'Files', [
        has(device, 'open-bucket') && act('open-bucket', 'Open synced files', 'folder-symbolic'),
        has(device, 'open-files') && act('open-files', 'Files page', 'folder-open-symbolic'),
        phoneSwitch(device, 'galleryAccept', 'Accept its photos and screenshots'),
    ]);
    const privacy = present([
        phoneSwitch(device, 'unlockNearby', 'Unlock with the phone'),
        phoneSwitch(device, 'walkAway', 'Lock when I walk away'),
        phoneSwitch(device, 'phoneKeepAwake', 'Phone can keep this awake'),
        deskSwitch(desk, 'deskMic', 'Phones can hear this mic'),
    ]);
    const onCount = [device.settings?.terminalAccess, device.settings?.unlockNearby,
        device.settings?.galleryAccept, device.settings?.phoneKeepAwake, device.settings?.glide,
        desk?.deskMic].filter(Boolean).length;
    add('privacy', 'channel-secure-symbolic', 'Privacy & security', [
        ...privacy,
        has(device, 'open-settings') && act('open-settings', 'All settings…', 'emblem-system-symbolic'),
        has(device, 'unpair') && act('unpair', `Unpair ${name}…`, 'edit-delete-symbolic', {danger: true}),
    ], onCount > 0 ? `${onCount} allowed` : '');
    add('computer', 'computer-symbolic', 'This computer', [
        deskSwitch(desk, 'keepAwake', 'Keep awake while a phone is connected'),
        desk?.handOff !== undefined && act('lock-hand-off', 'Lock & hand off', 'send-to-symbolic', {device: false, enabled: !!desk.handOff}),
        act('reconnect', 'Look for the phone again', 'view-refresh-symbolic', {device: false}),
    ]);
    return out;
}

/// What the menu shows, top to bottom. Rows for what is going on now come
/// only while something is: a call, music, files moving, the Files sync, the
/// phone's timers, a phone using this computer. Every area is folded into
/// More. A new kind of row is one more entry here and one more case in
/// menu.js's `fillMenu`.
export function menuBlocks(selected, devices = [], activities = []) {
    if (!selected)
        return ['empty'];
    const blocks = [];
    if (devices.length > 1)
        blocks.push('switcher');
    if (!selected.ready)
        blocks.push('offline');
    if (selected.call)
        blocks.push('call');
    if (selected.ready)
        blocks.push('tiles');
    if (selected.ready && selected.media)
        blocks.push('media');
    if ((selected.transfers ?? []).length > 0)
        blocks.push('transfers');
    if (selected.bucket)
        blocks.push('bucket');
    if (activities.some(item => item.device === selected.id))
        blocks.push('activities');
    if (devices.some(device => device.screen || device.terminals > 0))
        blocks.push('live');
    blocks.push('more', 'footer');
    return blocks;
}

/// The words beside the folded More row: what in it wants a look ("3 new",
/// "1 attached"); a switch that is on is not news.
export function moreSummary(list) {
    return list.filter(section => section.id === 'notifications' || section.id === 'terminals')
        .map(section => section.subtitle).filter(Boolean).join(' · ');
}

/// The footer's three buttons.
export function footer(device, desk) {
    const buttons = [
        {id: 'open', label: 'Open DeskLink', short: 'Open DeskLink', icon: 'focus-windows-symbolic', action: 'open'},
    ];
    if (has(device, 'open-settings'))
        buttons.push({id: 'settings', label: 'Settings', short: 'Settings', icon: 'emblem-system-symbolic', action: 'open-settings'});
    if (device?.settings && 'showNotifications' in device.settings) {
        const shown = device.settings.showNotifications;
        buttons.push({
            id: 'pause', label: shown ? 'Pause notifications' : 'Resume notifications',
            short: shown ? 'Pause' : 'Resume',
            icon: shown ? 'notifications-disabled-symbolic' : 'preferences-system-notifications-symbolic',
            key: 'showNotifications', on: !shown,
        });
    }
    return buttons;
}

// Endings that name a file, not a site (`README.md`): the same list as the app's
// `link_to_send` (src/window/overview.rs), which has the last word.
const FILE_ENDINGS = new Set(['md', 'txt', 'pdf', 'doc', 'docx', 'odt', 'xls', 'xlsx', 'ods', 'csv', 'ppt', 'pptx',
    'rtf', 'png', 'jpg', 'jpeg', 'gif', 'svg', 'webp', 'bmp', 'heic', 'mp3', 'mp4', 'mkv', 'wav', 'flac', 'zip',
    'tar', 'gz', 'xz', '7z', 'rs', 'py', 'js', 'ts', 'kt', 'java', 'sh', 'json', 'toml', 'yml', 'yaml', 'log', 'apk']);

function isHost(host, bare) {
    const labels = host.split('.');
    if (!labels.every(label => /^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(label)))
        return false;
    if (!bare)
        return true;
    const ending = labels.at(-1).toLowerCase();
    return labels.length >= 2 && /^[a-z]{2,}$/.test(ending) && !FILE_ENDINGS.has(ending);
}

/// Whether `text` is something to open on the phone: an http(s) address or a
/// bare `host.tld` (optional port and path). A file name (`README.md`), another
/// scheme (`file:///`, `javascript:`), a name with spaces or `user@host` is
/// not. The app checks it again.
export function looksLikeLink(text) {
    const typed = (text ?? '').trim();
    if (typed.length === 0 || typed.length > 2000 || /[\s\p{Cc}]/u.test(typed))
        return false;
    const scheme = /^(https?):\/\//i.exec(typed);
    if (!scheme && typed.includes('://'))
        return false;
    const rest = scheme ? typed.slice(scheme[0].length) : typed;
    const authority = rest.split(/[/?#]/, 1)[0];
    const colon = authority.lastIndexOf(':');
    const host = colon >= 0 ? authority.slice(0, colon) : authority;
    if (colon >= 0 && !/^[0-9]{1,5}$/.test(authority.slice(colon + 1)))
        return false;
    return isHost(host, !scheme);
}

/// Every action and switch key the menu can send, for the contract test.
export function everythingSent(device, desk, extras = {}) {
    const actions = new Set(['open', 'reconnect', 'try-again', 'end-sessions', 'media-play-pause', 'media-next', 'media-prev',
        'call-answer', 'call-decline', 'call-mute', 'call-unmute', 'call-end', 'new-note', 'lock-hand-off']);
    const keys = new Set();
    for (const tile of quickTiles(device))
        actions.add(tile.action);
    for (const section of sections(device, desk, {...extras, linkInClipboard: true})) {
        for (const row of section.rows) {
            if (row.kind === 'action')
                actions.add(row.action);
            if (row.kind === 'switch') {
                keys.add(row.key);
                if (row.sensitive)
                    actions.add('open-settings');
            }
        }
    }
    for (const button of footer(device, desk)) {
        if (button.action)
            actions.add(button.action);
        if (button.key)
            keys.add(button.key);
    }
    return {actions: [...actions], keys: [...keys]};
}

// ── The track's timeline ────────────────────────────────────────────────
/// "3:07", or "1:02:03" from an hour.
export function mediaClock(ms) {
    const seconds = Math.max(0, Math.floor((ms ?? 0) / 1000));
    const [h, m, s] = [Math.floor(seconds / 3600), Math.floor(seconds / 60) % 60, seconds % 60];
    return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${m}:${two(s)}`;
}

/// Where the track is: the position last reported (at `reportedAt`), moved on
/// while it plays, inside [0, length]. A seek in flight (`held`) wins.
export function mediaPosition(media, reportedAt, now, held = null) {
    if (held && now < held.until)
        return held.ms;
    const length = media.lengthMs > 0 ? media.lengthMs : Infinity;
    const moved = media.playing ? Math.max(0, now - reportedAt) : 0;
    return Math.min(length, Math.max(0, (media.positionMs ?? 0) + moved));
}

/// Why the timeline cannot be used, in words; '' when it can. Older apps (no
/// `lengthMs`) have no timeline at all: `null`.
export function seekReason(device, media) {
    if (!media || media.lengthMs === undefined)
        return null;
    if (media.seekReason)
        return media.seekReason;
    return device.actions.includes('media-seek') ? '' : 'Can\'t seek right now';
}

/// A dragged slider's values: at most one send per `every` ms, the last value
/// always going out (on a timer, or at once from `flush` on release).
export function seekThrottle({send, now, schedule, cancel, every = 200}) {
    let last = -Infinity, pending = null, timer = null;
    const fire = () => {
        timer = null;
        if (pending === null)
            return;
        const value = pending;
        pending = null;
        last = now();
        send(value);
    };
    return {
        change(value) {
            pending = value;
            const wait = every - (now() - last);
            if (wait <= 0) {
                if (timer !== null)
                    cancel(timer);
                fire();
            } else if (timer === null) {
                timer = schedule(wait, fire);
            }
        },
        flush() {
            if (timer !== null)
                cancel(timer);
            fire();
        },
    };
}
