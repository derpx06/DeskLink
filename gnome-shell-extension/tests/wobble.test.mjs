// Run with: node --test gnome-shell-extension/tests/wobble.test.mjs
import assert from 'node:assert/strict';
import test from 'node:test';

import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';

// The guard lives in extension.js (between the markers), which imports GNOME
// modules and so cannot be loaded here: the block is cut out and evaluated.
const source = readFileSync(
    fileURLToPath(new URL('../desklink@derx06.github.io/extension.js', import.meta.url)), 'utf8');
const block = source.split('// BEGIN wobble guard')[1].split('// END wobble guard')[0]
    .replace(/export /g, '');
const {stripWobble, WobbleGuard, WOBBLE_EFFECT} = new Function(
    `${block}; return {stripWobble, WobbleGuard, WOBBLE_EFFECT};`)();

function windowWith(effects) {
    const actor = {
        removed: [],
        get_effect: name => effects[name] ?? null,
        remove_effect(effect) { this.removed.push(effect); },
    };
    return {actor, get_compositor_private: () => actor};
}

test('the wobble effect is destroyed through its own method', () => {
    let destroyed = 0;
    const window = windowWith({[WOBBLE_EFFECT]: {destroy: () => destroyed++}});
    assert.equal(stripWobble(window), true);
    assert.equal(destroyed, 1);
    assert.deepEqual(window.actor.removed, []);
});

test('an effect that cannot destroy itself is detached, and others are left alone', () => {
    const odd = {destroy: () => { throw new Error('no'); }};
    const window = windowWith({[WOBBLE_EFFECT]: odd, 'burn-my-windows': {}});
    assert.equal(stripWobble(window), true);
    assert.deepEqual(window.actor.removed, [odd]);
    assert.equal(stripWobble(windowWith({'burn-my-windows': {}})), false);
    assert.equal(stripWobble(null), false);
    assert.equal(stripWobble({get_compositor_private: () => null}), false);
});

test('the guard listens only while a phone is looking, and after the other handlers', () => {
    const calls = [];
    const display = {
        connect_after: (signal, handler) => { calls.push(['connect_after', signal]); display.handler = handler; return 7; },
        disconnect: id => calls.push(['disconnect', id]),
    };
    const guard = new WobbleGuard(display);
    guard.sharing = false;
    assert.deepEqual(calls, []);
    guard.sharing = true;
    guard.sharing = true;
    assert.deepEqual(calls, [['connect_after', 'grab-op-begin']]);

    let destroyed = 0;
    display.handler(display, windowWith({[WOBBLE_EFFECT]: {destroy: () => destroyed++}}), 1);
    assert.equal(destroyed, 1);

    guard.sharing = false;
    guard.sharing = false;
    assert.deepEqual(calls.slice(1), [['disconnect', 7]]);
    guard.sharing = true;
    guard.destroy();
    assert.deepEqual(calls.slice(-1), [['disconnect', 7]]);
});
