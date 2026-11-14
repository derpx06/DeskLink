// Run with: node --test gnome-shell-extension/tests/
import assert from 'node:assert/strict';
import test from 'node:test';
import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';

import {
    everythingSent, footer, headerLine, lastSeen, mediaClock, mediaPosition, menuBlocks, moreSummary, seekReason, seekThrottle,
    looksLikeLink, quickTiles, sections, sessionLine, shortStatus, statusLine, timer, duration,
} from '../desklink@derx06.github.io/model.js';

const ALL_ACTIONS = ['open', 'ring', 'lock', 'send-files', 'send-link', 'send-clipboard', 'ping', 'show-screen',
    'new-terminal', 'insert-photo', 'insert-photo-file', 'insert-scan', 'insert-text', 'insert-qr',
    'open-files', 'open-notes', 'open-clipboard', 'open-snippets', 'open-notifications', 'open-contacts',
    'open-terminals', 'open-settings', 'unpair', 'open-bucket', 'end-sessions', 'media-prev'];

const phone = (extra = {}) => ({
    id: 'p1', name: 'Pixel', paired: true, reachable: true, ready: true, battery: 82, charging: true,
    path: 'lan', screen: '', terminals: 0, lastSeen: 0, rttMs: 12, unread: 3, textCapture: true,
    transfers: [], sound: {deskSound: false, phoneMic: false, phoneSound: false},
    settings: {
        showNotifications: true, dndShare: false, commandAlerts: true, namesCallers: false, walkAway: false,
        terminalAccess: false, unlockNearby: false, galleryAccept: false, phoneKeepAwake: false, glide: false,
    },
    actions: ALL_ACTIONS, ...extra,
});
const desk = (extra = {}) => ({
    keepAwake: false, clipboard: true, handOff: false, pill: false, forwardNotifications: false,
    pauseMediaOnCall: true, deskMic: false, soundAvailable: true, dndSupported: true, hasPassword: false, ...extra,
});
const ids = list => list.map(item => item.id);
const section = (list, id) => list.find(entry => entry.id === id);

test('the status line says it in plain words', () => {
    assert.equal(statusLine(phone()), 'Connected · Wi-Fi · 82% ⚡');
    assert.equal(statusLine(phone({ready: false})), 'Connecting…');
    assert.equal(statusLine(phone({ready: false, reachable: false})), 'Not connected');
    assert.equal(statusLine(null), 'No phone connected');
    assert.equal(shortStatus(phone({charging: false, path: 'turn'})), 'Relay · 82%');
});

test('last seen shows the time today and the day before', () => {
    const now = new Date(2026, 10, 5, 15, 0);
    const at = d => Math.floor(d.getTime() / 1000);
    assert.equal(lastSeen({lastSeen: at(new Date(2026, 10, 5, 14, 2))}, now), 'last seen 14:02');
    assert.equal(lastSeen({lastSeen: at(new Date(2026, 10, 3, 9, 5))}, now), 'last seen 3 Nov 09:05');
    assert.equal(lastSeen({lastSeen: 0}, now), '');
});

test('the header line: the link, the route with its round trip, charging', () => {
    assert.equal(headerLine(phone()), 'Connected · Wi-Fi · 12 ms · charging');
    assert.equal(headerLine(phone({charging: false, path: 'turn', rttMs: -1})), 'Connected · Relay');
    assert.equal(headerLine(phone({path: 'unknown', charging: false})), 'Connected');
    assert.equal(headerLine(phone({ready: false})), 'Connecting…');
    assert.equal(headerLine(phone({ready: false, reachable: false})), 'Not connected');
    assert.equal(headerLine(null), 'No phone connected');
});

test('the six tiles follow what the phone takes and whether it is connected', () => {
    const tiles = quickTiles(phone());
    assert.deepEqual(ids(tiles), ['send-files', 'send-clipboard', 'ring', 'lock', 'show-screen', 'insert-photo']);
    assert.ok(tiles.every(tile => tile.enabled));
    const without = quickTiles(phone({actions: ['open', 'ring']}));
    assert.deepEqual(without.filter(t => t.enabled).map(t => t.id), ['ring']);
    assert.ok(quickTiles(phone({ready: false})).every(tile => !tile.enabled));
});

test('a connected phone gets every section, in a fixed order', () => {
    const list = sections(phone(), desk(), {snippets: [], transfers: []});
    assert.deepEqual(ids(list), ['send', 'insert', 'sound', 'notes', 'notifications', 'calls', 'terminals',
        'screen', 'files', 'privacy', 'computer']);
});

test('a section appears only when the phone offers something in it', () => {
    const bare = phone({actions: ['open'], sound: undefined});
    const list = sections(bare, desk({deskMic: undefined}), {});
    assert.ok(!section(list, 'send'));
    assert.ok(!section(list, 'insert'));
    assert.ok(!section(list, 'sound'));
    assert.deepEqual(ids(section(list, 'screen').rows), ['glide'], 'only the setting is left');
    // What is configuration still shows while the phone is offline.
    assert.ok(section(list, 'notifications'));
    assert.ok(section(list, 'privacy'));
    assert.deepEqual(sections(null, desk()), []);
});

test('an older app (no settings) shows no switches it cannot set', () => {
    const old = phone({settings: undefined, sound: undefined, actions: ['open', 'ring']});
    const list = sections(old, {keepAwake: true, clipboard: false, handOff: false, pill: false}, {});
    const switches = list.flatMap(entry => entry.rows).filter(row => row.kind === 'switch');
    assert.deepEqual(switches.map(row => row.key).sort(), ['clipboard', 'keepAwake']);
});

test('security switches are marked, and the unlock one is among them', () => {
    const list = sections(phone(), desk(), {});
    const rows = list.flatMap(entry => entry.rows).filter(row => row.kind === 'switch');
    const sensitive = rows.filter(row => row.sensitive).map(row => row.key).sort();
    assert.deepEqual(sensitive, ['deskMic', 'galleryAccept', 'glide', 'phoneKeepAwake', 'terminalAccess', 'unlockNearby']);
    // Walk-away only ever locks; it is not one of them.
    assert.equal(rows.find(row => row.key === 'walkAway').sensitive, false);
    assert.equal(rows.find(row => row.key === 'deskMic').scope, 'desk');
    assert.equal(rows.find(row => row.key === 'terminalAccess').scope, 'phone');
});

test('a switch shows the app\'s state', () => {
    const list = sections(phone({settings: {...phone().settings, terminalAccess: true, dndShare: true}}), desk(), {});
    const row = key => list.flatMap(entry => entry.rows).find(r => r.key === key);
    assert.equal(row('terminalAccess').on, true);
    assert.equal(row('dndShare').on, true);
    assert.equal(row('showNotifications').on, true);
    assert.equal(row('walkAway').on, false);
    assert.equal(row('pauseMediaOnCall').on, true);
    assert.equal(section(list, 'privacy').subtitle, '1 allowed');
});

test('sound switches need a connection and a working sound system', () => {
    const rows = (device, d) => section(sections(device, d, {}), 'sound').rows;
    assert.ok(rows(phone(), desk()).every(row => row.enabled));
    assert.ok(rows(phone({ready: false}), desk()).every(row => !row.enabled));
    const off = rows(phone(), desk({soundAvailable: false}));
    assert.ok(off.every(row => !row.enabled));
    assert.match(off[0].note, /Not available/);
    // One that is already on can always be turned off.
    const on = rows(phone({ready: false, sound: {deskSound: true}}), desk());
    assert.equal(on.find(row => row.key === 'deskSound').enabled, true);
});

test('Do Not Disturb says when this desktop cannot share it', () => {
    const row = section(sections(phone(), desk({dndSupported: false}), {}), 'notifications')
        .rows.find(r => r.key === 'dndShare');
    assert.equal(row.enabled, false);
    assert.match(row.note, /desktop/);
});

test('Send copied link waits for a link on the clipboard', () => {
    const find = linkInClipboard => section(sections(phone(), desk(), {linkInClipboard}), 'send')
        .rows.find(row => row.id === 'send-link');
    assert.equal(find(true).enabled, true);
    assert.equal(find(false).enabled, false);
});

// The same vectors as `only_web_addresses_and_site_names_become_links` in
// src/window/overview.rs.
test('only web addresses and site names are links', () => {
    for (const typed of ['https://example.org/a?b=c', 'http://example.org', 'example.org', ' example.org ',
        'www.example.com/path?x=1', 'sub.example.co.uk', 'example.org:8080/x', 'HTTPS://Example.org',
        'http://localhost:3000'])
        assert.ok(looksLikeLink(typed), typed);
    for (const typed of ['README.md', 'notes.txt', 'photo.jpg', 'foo.md/bar', 'archive.tar.gz',
        'file:///etc/passwd', 'ftp://example.org', 'javascript:alert.x', 'mailto:a@b.org', 'data:text/plain,hi',
        'https://', 'http:///x', 'https://exa mple.org', 'hello', '1.2.3', '192.168.1.1', 'a..b.org', '-a.org',
        'example.org:abc', 'example.org:', 'https://good.com@evil.org', '[::1]:80', 'two words.org', '', null])
        assert.ok(!looksLikeLink(typed), String(typed));
});

test('recent files and snippets are rows of their sections, capped', () => {
    const files = Array.from({length: 5}, (_, i) => ({id: `/f${i}`, name: `f${i}.txt`, incoming: i % 2 === 0, deviceName: 'Pixel'}));
    const snippets = Array.from({length: 9}, (_, i) => ({id: `s${i}`, label: `S${i}`, text: `t${i}`}));
    const list = sections(phone(), desk(), {transfers: files, snippets});
    assert.equal(section(list, 'send').rows.filter(r => r.kind === 'file').length, 3);
    assert.equal(section(list, 'notes').rows.filter(r => r.kind === 'snippet').length, 5);
    assert.equal(section(list, 'send').rows.find(r => r.kind === 'file').detail, 'from Pixel');
});

test('the footer: Open DeskLink, Settings, and Pause that turns into Resume', () => {
    assert.deepEqual(footer(phone(), desk()).map(b => b.label), ['Open DeskLink', 'Settings', 'Pause notifications']);
    const paused = footer(phone({settings: {showNotifications: false}}), desk());
    assert.equal(paused[2].label, 'Resume notifications');
    assert.equal(paused[2].on, true);
    assert.deepEqual(footer(null, desk()).map(b => b.label), ['Open DeskLink']);
});

test('notifications show how many are new', () => {
    const list = sections(phone({unread: 2}), desk(), {});
    const open = section(list, 'notifications').rows.find(r => r.id === 'open-notifications');
    assert.equal(open.label, 'Open notifications (2 new)');
    assert.equal(section(list, 'notifications').subtitle, '2 new');
});

test('live words and clocks', () => {
    assert.equal(sessionLine({screen: 'view', terminals: 2}), 'viewing the screen · 2 terminals');
    assert.equal(timer(0, 65000), '1:05');
    assert.equal(timer(0, 3723000), '1:02:03');
    assert.equal(duration(0, 30000), 'just now');
    assert.equal(duration(0, 3900 * 1000), '1 h 05 min');
});

// The contract: everything the menu sends exists in the app's interface.
const rust = readFileSync(
    fileURLToPath(new URL('../../src/shell_dbus.rs', import.meta.url)), 'utf8').split('#[cfg(test)]')[0];

/// The text of one item of shell_dbus.rs, from its first line to its `end`.
function part(start, end) {
    const from = rust.indexOf(start);
    assert.ok(from >= 0, `${start} is not in shell_dbus.rs`);
    return rust.slice(from, rust.indexOf(end, from));
}
const quoted = text => [...text.matchAll(/"([a-z][a-z-]*)"/g)].map(m => m[1]);

test('every action the menu sends is in the table that serves it', () => {
    const deviceless = new Set(quoted(part('const DEVICELESS', '];')));
    const inserts = new Set(quoted(part('const INSERTS', '];')));
    const pages = new Set([...part('const PAGES', '];').matchAll(/\("([a-z-]+)", "/g)].map(m => m[1]));
    // actions(): what a phone is offered; read between its signature and the next item.
    const offered = new Set(quoted(part('fn actions(device: &DeviceView', '\n}\n')));
    for (const table of [deviceless, inserts, pages, offered])
        assert.ok(table.size >= 3, 'a table was read');

    const rows = sections(phone(), desk(), {
        snippets: [{id: 's', label: 'S', text: 't'}],
        transfers: [{id: '/f', name: 'f', incoming: true, deviceName: 'Pixel'}],
        linkInClipboard: true,
    }).flatMap(entry => entry.rows).filter(row => row.kind === 'action');
    const forDevice = [...quickTiles(phone()), ...rows].filter(row => row.device !== false).map(row => row.action);
    const forDesk = rows.filter(row => row.device === false).map(row => row.action);
    assert.deepEqual(forDesk.sort(), ['lock-hand-off', 'new-note', 'reconnect']);
    for (const action of forDesk)
        assert.ok(deviceless.has(action), `${action} is not in DEVICELESS`);
    for (const action of forDevice) {
        assert.ok(offered.has(action) || inserts.has(action) || pages.has(action),
            `${action} is not offered by actions(), INSERTS or PAGES`);
        assert.ok(!deviceless.has(action) || action === 'end-sessions', `${action} would be sent with a device but is device-less`);
    }
    // What the cards and the footer send, each in the table that serves it.
    for (const action of ['call-answer', 'call-decline', 'call-mute', 'call-unmute', 'call-end',
        'media-prev', 'media-play-pause', 'media-next', 'try-again', 'end-sessions', 'open-bucket', 'open', 'open-settings'])
        assert.ok(offered.has(action) || pages.has(action), `${action} is not offered by actions() or PAGES`);
    for (const action of ['reconnect', 'end-sessions'])
        assert.ok(deviceless.has(action), `${action} is not in DEVICELESS`);
    // Every action the menu can send is one of those.
    const {actions} = everythingSent(phone(), desk(), {});
    for (const action of actions)
        assert.ok(offered.has(action) || inserts.has(action) || pages.has(action) || deviceless.has(action), action);
});

test('every switch key the menu sends is in the app\'s allow-list', () => {
    const {keys} = everythingSent(phone(), desk(), {});
    for (const key of keys)
        assert.ok(rust.includes(`switch("${key}"`), `${key} is not in SWITCHES`);
});

test('the app\'s published switches are all shown somewhere', () => {
    const {keys} = everythingSent(phone(), desk(), {});
    const listed = [...rust.matchAll(/switch\("(\w+)", Scope::(Phone|Desk), (true|false)\)/g)];
    assert.ok(listed.length >= 18);
    for (const [, key, , sensitive] of listed) {
        assert.ok(keys.includes(key), `${key} is not in the menu`);
        const row = sections(phone(), desk(), {}).flatMap(s => s.rows).find(r => r.key === key);
        if (row)
            assert.equal(row.sensitive, sensitive === 'true', `${key}: sensitivity differs from the app`);
    }
});

test('every action row is usable unless it says otherwise', () => {
    const rows = sections(phone(), desk(), {linkInClipboard: false}).flatMap(s => s.rows).filter(r => r.kind === 'action');
    assert.ok(rows.length > 20);
    // The link waits for a link on the clipboard, Hand Off for a running terminal.
    assert.deepEqual(rows.filter(r => !r.enabled).map(r => r.id), ['send-link', 'lock-hand-off']);
});

// The compact menu: only what is going on now is shown beyond the tiles; the
// rest is folded into More.
test('an idle phone is the tiles, More and the footer', () => {
    assert.deepEqual(menuBlocks(phone(), [phone()]), ['tiles', 'more', 'footer']);
    assert.deepEqual(menuBlocks(null, []), ['empty']);
});

test('rows for what is going on come only while it is', () => {
    const busy = phone({
        media: {title: 't', playing: true}, call: {id: 'c', state: 'ringing', incoming: true},
        transfers: [{name: 'a', fraction: 0.5}], bucket: {line: 'Files', fraction: 0.2}, screen: 'view',
    });
    assert.deepEqual(menuBlocks(busy, [busy], [{device: 'p1', title: 'Pasta'}]),
        ['call', 'tiles', 'media', 'transfers', 'bucket', 'activities', 'live', 'more', 'footer']);
    // Another phone's timer is not this one's; another phone using this computer is.
    const other = phone({id: 'p2', terminals: 1});
    assert.deepEqual(menuBlocks(phone(), [phone(), other], [{device: 'p2', title: 'x'}]),
        ['switcher', 'tiles', 'live', 'more', 'footer']);
    // Music from a phone that is away is not playing here.
    assert.ok(!menuBlocks(phone({ready: false, media: {title: 't'}}), [phone()]).includes('media'));
});

test('a phone that is away gets a way back instead of greyed-out tiles', () => {
    assert.deepEqual(menuBlocks(phone({ready: false, reachable: false}), [phone()]), ['offline', 'more', 'footer']);
});

test('More says what in it wants a look', () => {
    assert.equal(moreSummary(sections(phone(), desk(), {})), '3 new');
    assert.equal(moreSummary(sections(phone({unread: 0, terminals: 2}), desk(), {})), '2 attached');
    assert.equal(moreSummary(sections(phone({unread: 0, sound: {deskSound: true}}), desk(), {})), '');
});

// Folding everything into More must not lose anything: the actions and
// switches the menu can send are exactly those of the menu before it was made
// compact (2026-11-14).
test('every action and switch is still reachable', () => {
    const extras = {snippets: [{id: 's', label: 'S', text: 't'}], transfers: [{id: '/f', name: 'f', incoming: true, deviceName: 'Pixel'}]};
    const {actions, keys} = everythingSent(phone(), desk(), extras);
    assert.deepEqual(actions.sort(), ['call-answer', 'call-decline', 'call-end', 'call-mute', 'call-unmute', 'end-sessions',
        'insert-photo', 'insert-photo-file', 'insert-qr', 'insert-scan', 'insert-text', 'lock', 'lock-hand-off', 'media-next',
        'media-play-pause', 'media-prev', 'new-note', 'new-terminal', 'open', 'open-bucket', 'open-clipboard', 'open-contacts',
        'open-files', 'open-notes', 'open-notifications', 'open-settings', 'open-terminals', 'ping', 'reconnect', 'ring',
        'send-clipboard', 'send-files', 'send-link', 'show-screen', 'try-again', 'unpair']);
    assert.deepEqual(keys.sort(), ['clipboard', 'commandAlerts', 'deskMic', 'deskSound', 'dndShare', 'forwardNotifications',
        'galleryAccept', 'glide', 'keepAwake', 'namesCallers', 'pauseMediaOnCall', 'phoneKeepAwake', 'phoneMic', 'phoneSound',
        'showNotifications', 'terminalAccess', 'unlockNearby', 'walkAway']);
    const rows = sections(phone(), desk(), {...extras, linkInClipboard: true}).flatMap(s => s.rows);
    assert.equal(rows.length, 44, 'every row of every area is in More');
    // The blocks that draw the rest are all there for a phone that has them.
    const busy = phone({media: {title: 't'}, call: {id: 'c'}, transfers: [{}], bucket: {}, screen: 'view'});
    for (const block of ['tiles', 'media', 'call', 'transfers', 'bucket', 'live', 'more', 'footer'])
        assert.ok(menuBlocks(busy, [busy]).includes(block), block);
    assert.ok(menuBlocks(phone({ready: false}), [phone()]).includes('offline'));
});

// menu.js draws every block menuBlocks can return (a new kind of row needs both).
test('the menu draws every kind of block', () => {
    const menu = readFileSync(fileURLToPath(new URL('../desklink@derx06.github.io/menu.js', import.meta.url)), 'utf8');
    const model = readFileSync(fileURLToPath(new URL('../desklink@derx06.github.io/model.js', import.meta.url)), 'utf8');
    const body = model.slice(model.indexOf('export function menuBlocks'), model.indexOf('export function moreSummary'));
    const kinds = [...body.matchAll(/'([a-z]+)'/g)].map(m => m[1]);
    assert.ok(kinds.length >= 10);
    for (const kind of kinds.filter(k => k !== 'empty'))
        assert.ok(menu.includes(`case '${kind}':`), `menu.js draws no '${kind}'`);
});

test('the timeline moves on while playing, stays inside the track and yields to a seek in flight', () => {
    const media = {playing: true, positionMs: 10_000, lengthMs: 200_000};
    assert.equal(mediaPosition(media, 1000, 4000), 13_000);
    assert.equal(mediaPosition({...media, playing: false}, 1000, 4000), 10_000);
    assert.equal(mediaPosition({...media, positionMs: 199_000}, 0, 9000), 200_000);
    assert.equal(mediaPosition({...media, lengthMs: 0}, 0, 9000), 19_000);
    // Just sought to 90 s: the old report is not the position, for a moment.
    assert.equal(mediaPosition(media, 1000, 2000, {ms: 90_000, until: 2500}), 90_000);
    assert.equal(mediaPosition(media, 1000, 3000, {ms: 90_000, until: 2500}), 12_000);
    assert.equal(mediaClock(187_000), '3:07');
    assert.equal(mediaClock(3_723_000), '1:02:03');
});

test('a stream, a refusing player and an older app say why the timeline is off', () => {
    const device = phone({actions: [...ALL_ACTIONS, 'media-seek']});
    assert.equal(seekReason(device, {lengthMs: 200_000, seekReason: ''}), '');
    assert.match(seekReason(device, {lengthMs: 0, seekReason: "Can't seek: live"}), /live/);
    assert.equal(seekReason(device, {title: 'x'}), null); // an app older than interface 10: no bar
    assert.match(seekReason(phone(), {lengthMs: 9, seekReason: ''}), /Can't seek/);
});

test('a dragged slider sends five a second at most, and the release sends the last value', () => {
    let t = 0;
    const sent = [], timers = [];
    const throttle = seekThrottle({
        send: v => sent.push(v), now: () => t,
        schedule: (ms, fn) => timers.push({at: t + ms, fn}) , cancel: id => timers.splice(timers.indexOf(id), 1),
    });
    throttle.change(1); // goes out at once
    for (const [at, v] of [[50, 2], [100, 3], [150, 4]]) {
        t = at;
        throttle.change(v);
    }
    assert.deepEqual(sent, [1]);
    assert.equal(timers.length, 1); // one trailing send, not three
    t = 200;
    timers.shift().fn();
    assert.deepEqual(sent, [1, 4]);
    t = 230;
    throttle.change(5);
    throttle.flush(); // released: no waiting
    assert.deepEqual(sent, [1, 4, 5]);
    throttle.flush(); // nothing new: nothing sent
    assert.deepEqual(sent, [1, 4, 5]);
});
