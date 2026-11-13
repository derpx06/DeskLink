// Run with: node --test gnome-shell-extension/tests/notifications.test.mjs
import assert from 'node:assert/strict';
import test from 'node:test';

import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';

// The plan lives in notifications.js (between the markers), which imports
// GNOME modules and so cannot be loaded here: the block is cut out and evaluated.
const source = readFileSync(
    fileURLToPath(new URL('../desklink@derx06.github.io/notifications.js', import.meta.url)), 'utf8');
const block = source.split('// BEGIN notify plan')[1].split('// END notify plan')[0];
const {NotifyPlan} = new Function(`${block}; return {NotifyPlan};`)();

const entry = (id, body, extra = {}) => ({
    device: 'p', deviceName: 'Phone', id, app: 'Chat', title: 'Mara', body,
    actions: [], replyable: true, dismissable: true, silent: false, ready: true, ...extra,
});
const banners = ops => ops.filter(op => op.op === 'show' && !op.quiet);

test('the first read and a resync never banner', () => {
    const plan = new NotifyPlan();
    const ops = plan.sync([entry('a', 'hi'), entry('b', 'yo', {silent: true})], true, 0);
    assert.equal(ops.length, 2);
    assert.equal(banners(ops).length, 0);
    // Polled again unchanged: nothing.
    assert.deepEqual(plan.sync([entry('a', 'hi'), entry('b', 'yo', {silent: true})], false, 1000), []);
});

test('a new one banners; new words replace it, never add', () => {
    const plan = new NotifyPlan();
    plan.sync([], true, 0);
    let ops = plan.sync([entry('a', 'one')], false, 1000);
    assert.equal(banners(ops).length, 1);
    assert.equal(ops[0].replace, false);
    ops = plan.sync([entry('a', 'two')], false, 20000);
    assert.equal(ops.length, 1);
    assert.equal(ops[0].replace, true, 'replaces the one shown');
    assert.equal(ops[0].quiet, false);
    assert.deepEqual(plan.ids(), ['p\u0000a']);
});

test('the link dropping and coming back redraws quietly, never a banner', () => {
    const plan = new NotifyPlan();
    plan.sync([entry('a', 'one')], false, 0);
    let ops = plan.sync([entry('a', 'one', {ready: false})], false, 20000);
    assert.equal(ops.length, 1);
    assert.equal(ops[0].replace, true);
    assert.equal(ops[0].quiet, true);
    ops = plan.sync([entry('a', 'one', {ready: true, silent: true})], false, 40000);
    assert.equal(banners(ops).length, 0);
});

test('what the person closed stays closed until its words change', () => {
    const plan = new NotifyPlan();
    plan.sync([entry('a', 'one')], false, 0);
    plan.closedHere('p\u0000a');
    assert.deepEqual(plan.sync([entry('a', 'one')], false, 1000), []);
    assert.deepEqual(plan.sync([entry('a', 'one', {ready: false})], false, 2000), [], 'a link change does not bring it back');
    const ops = plan.sync([entry('a', 'new words')], false, 30000);
    assert.equal(banners(ops).length, 1);
});

test('removed by the phone is withdrawn; re-posted at once is quiet, later it banners', () => {
    const plan = new NotifyPlan();
    plan.sync([entry('a', 'one')], false, 0);
    assert.deepEqual(plan.sync([], false, 20000).map(op => op.op), ['withdraw']);
    assert.equal(banners(plan.sync([entry('a', 'one')], false, 21000)).length, 0, 're-post');
    plan.sync([], false, 22000);
    assert.equal(banners(plan.sync([entry('a', 'one')], false, 90000)).length, 1, 'a real new post');
});

test('a counter ticking banners once a minute at most', () => {
    const plan = new NotifyPlan();
    let count = 0;
    for (let t = 0; t < 30; t++)
        count += banners(plan.sync([entry('d', `Downloading ${t * 3}%`)], false, 100000 + t * 1000)).length;
    assert.equal(count, 1);
});

test('a burst is at most five banners, and the same words under two ids are one', () => {
    const plan = new NotifyPlan();
    const many = Array.from({length: 12}, (_, n) => entry(`n${n}`, `message ${n}`));
    assert.equal(banners(plan.sync(many, false, 0)).length, 5);
    const twins = new NotifyPlan();
    assert.equal(banners(twins.sync([entry('x', 'twin'), entry('y', 'twin')], false, 0)).length, 1);
});

test('every decision is traced', () => {
    const lines = [];
    const plan = new NotifyPlan(line => lines.push(line));
    plan.sync([entry('a', 'one')], false, 0);
    assert.match(lines[0], /added with a banner/);
});
