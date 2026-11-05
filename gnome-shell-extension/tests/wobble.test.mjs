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
const {stripActor, stripWobble, lookingAtScreen, WobbleGuard, WOBBLE_EFFECT} = new Function(
    `${block}; return {stripActor, stripWobble, lookingAtScreen, WobbleGuard, WOBBLE_EFFECT};`)();

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

test('an effect that cannot even be detached does not throw', () => {
    const actor = {
        get_effect: () => ({destroy: () => { throw new Error('no'); }}),
        remove_effect: () => { throw new Error('gone'); },
    };
    assert.equal(stripActor(actor), true);
    assert.equal(stripActor(null), false);
});

test('only a phone viewing or controlling counts as looking at the screen', () => {
    assert.equal(lookingAtScreen(undefined), false);
    assert.equal(lookingAtScreen(null), false);
    assert.equal(lookingAtScreen([]), false);
    assert.equal(lookingAtScreen([{screen: ''}, {screen: 'glide'}, null]), false);
    assert.equal(lookingAtScreen([{screen: 'glide'}, {screen: 'view'}]), true);
    assert.equal(lookingAtScreen([{screen: ''}, {screen: 'control'}]), true);
});

function signalSource(calls, name, base) {
    let next = base;
    return {
        connect_after: (signal, handler) => {
            calls.push([name, 'connect_after', signal]);
            next += 1;
            return next;
        },
        disconnect: id => calls.push([name, 'disconnect', id]),
    };
}

test('the guard listens only while a phone is looking, to a grab and to size changes, after the others', () => {
    const calls = [];
    const handlers = {};
    const display = signalSource(calls, 'display', 10);
    const wm = signalSource(calls, 'wm', 20);
    const original = display.connect_after;
    display.connect_after = (signal, handler) => { handlers[signal] = handler; return original(signal, handler); };
    const originalWm = wm.connect_after;
    wm.connect_after = (signal, handler) => { handlers[signal] = handler; return originalWm(signal, handler); };
    const guard = new WobbleGuard(display, wm);
    guard.sharing = false;
    assert.deepEqual(calls, []);
    guard.sharing = true;
    guard.sharing = true;
    assert.deepEqual(calls, [
        ['display', 'connect_after', 'grab-op-begin'],
        ['wm', 'connect_after', 'size-change'],
        ['wm', 'connect_after', 'size-changed'],
    ]);

    let destroyed = 0;
    const actor = {get_effect: () => ({destroy: () => destroyed++})};
    handlers['grab-op-begin'](display, {get_compositor_private: () => actor}, 1);
    handlers['size-change'](wm, actor, 3);
    handlers['size-changed'](wm, actor);
    assert.equal(destroyed, 3);

    calls.length = 0;
    guard.sharing = false;
    guard.sharing = false;
    assert.deepEqual(calls, [['display', 'disconnect', 11], ['wm', 'disconnect', 21], ['wm', 'disconnect', 22]]);
    calls.length = 0;
    guard.sharing = true;
    calls.length = 0;
    guard.destroy();
    assert.equal(calls.length, 3);
});
