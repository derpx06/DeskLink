// Run with: node --test gnome-shell-extension/tests/activity.test.mjs
import assert from 'node:assert/strict';
import test from 'node:test';

import {clockText, liveItems, liveText} from '../desklink@derx06.github.io/activity.js';

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
