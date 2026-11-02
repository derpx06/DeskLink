// Run with: node --test gnome-shell-extension/tests/activity.test.mjs
import assert from 'node:assert/strict';
import test from 'node:test';

import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';

// The helpers live in extension.js (between the markers), which imports GNOME
// modules and so cannot be loaded here: the block is cut out and evaluated.
const source = readFileSync(
    fileURLToPath(new URL('../desklink@derx06.github.io/extension.js', import.meta.url)), 'utf8');
const block = source.split('// BEGIN activity helpers')[1].split('\n').slice(1).join('\n')
    .split('// END activity helpers')[0]
    .replace(/export function/g, 'function');
const {chipLabel, clockText, liveItems, liveText} = new Function(
    `${block}; return {chipLabel, clockText, liveItems, liveText};`)();

test('a countdown reads minutes and seconds, rounding up', () => {
    assert.equal(clockText(272000), '4:32');
    assert.equal(clockText(400), '0:01');
    assert.equal(clockText(3661000), '1:01:01');
    assert.equal(liveText({base: 10272000, countdown: true, progress: -1}, 10000000), '4:32 left');
    assert.equal(liveText({base: 10000000, countdown: true, progress: -1}, 10000000), 'Done');
});

test('a count-up and a percentage', () => {
    assert.equal(liveText({base: 10000000, countdown: false, progress: -1}, 10065000), '1:05 elapsed');
    assert.equal(liveText({base: -1, countdown: false, progress: 42}, 1), '42%');
    assert.equal(liveText({base: -1, countdown: false, progress: -1}, 1), '');
});

test('a countdown over for a minute is dropped, a count-up stays', () => {
    const items = [
        {id: 'a', base: 1000, countdown: true},
        {id: 'b', base: 1000, countdown: false},
        {id: 'c', base: 50000, countdown: true},
    ];
    assert.deepEqual(liveItems(items, 61001).map(item => item.id), ['b', 'c']);
});

test('the pill text is cut to a width the panel can take', () => {
    assert.equal(chipLabel('Pasta 4:32 left'), 'Pasta 4:32 left');
    const long = chipLabel('Turn left onto Hauptstrasse and then keep right at the fork 4:32 left');
    assert.equal(Array.from(long).length, 28);
    assert.ok(long.endsWith('…'));
    assert.equal(Array.from(chipLabel('é'.repeat(100))).length, 28);
});
