import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/pendingDeletes.ts', import.meta.url), 'utf8'));
const {PendingDeletes} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('deletion hides immediately, before network work, and commits before clearing the mask', async () => {
  const events = [];
  let finish;
  const pending = new PendingDeletes(keys => events.push([...keys]));
  const request = pending.run('clip:a', () => new Promise(resolve => {
    events.push('network'); finish = resolve;
  }), () => events.push('committed'));
  assert.deepEqual(events, [['clip:a']]);
  await Promise.resolve();
  assert.deepEqual(events, [['clip:a'], 'network']);
  finish();
  await request;
  assert.deepEqual(events, [['clip:a'], 'network', 'committed', []]);
});

test('failed deletion restores its row without unhiding another pending deletion', async () => {
  let visible;
  const pending = new PendingDeletes(keys => {visible = keys;});
  let finish;
  const other = pending.run('image:b', () => new Promise(resolve => {finish = resolve;}), () => {});
  const failed = pending.run('clip:a', async () => {throw new Error('read-only share');}, () => assert.fail('failed delete committed'));
  await assert.rejects(failed, /read-only/);
  assert.deepEqual([...visible], ['image:b']);
  finish(); await other;
  assert.equal(visible.size, 0);
});

test('duplicate confirmation shares one request', async () => {
  let calls = 0;
  const pending = new PendingDeletes(() => {});
  const first = pending.run('clip:a', async () => {calls++;}, () => {});
  assert.equal(pending.run('clip:a', () => assert.fail('duplicate request'), () => {}), first);
  await first;
  assert.equal(calls, 1);
});
