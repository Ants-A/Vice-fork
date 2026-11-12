import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';

const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/playbackSessions.ts', import.meta.url), 'utf8'));
const {PlaybackSessions} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('viewer and trim retain one session until the last player closes', async t => {
  t.mock.timers.enable({apis: ['setInterval']});
  const calls = [];
  const sessions = new PlaybackSessions({
    create: async slug => {calls.push(['create', slug]); return {id: 'session-1'};},
    touch: async id => {calls.push(['touch', id]);},
    close: async id => {calls.push(['close', id]);},
  });
  const viewer = sessions.acquire('clip', '/v/clip?v=revision');
  assert.equal(await viewer.source, '/v/clip?v=revision&playback_session=session-1');
  // Finishing a video does not release its lease or change its URL.
  t.mock.timers.tick(90_000);
  assert.equal(calls.filter(call => call[0] === 'close').length, 0);
  const trim = sessions.acquire('clip', '/v/clip?v=revision');
  assert.equal(await trim.source, await viewer.source);
  viewer.release();
  viewer.release();
  assert.equal(calls.filter(call => call[0] === 'close').length, 0);
  trim.release();
  const before = calls.length;
  t.mock.timers.tick(90_000);
  assert.equal(calls.length, before, 'closing must stop the heartbeat');
  assert.equal(calls.filter(call => call[0] === 'create').length, 1);
  assert.deepEqual(calls.at(-1), ['close', 'session-1']);
});

test('closing before session creation finishes releases the late session', async () => {
  let complete;
  const closed = [];
  const sessions = new PlaybackSessions({
    create: () => new Promise(resolve => {complete = resolve;}),
    touch: async () => {},
    close: async id => {closed.push(id);},
  });
  const viewer = sessions.acquire('clip', '/v/clip');
  viewer.release();
  complete({id: 'late'});
  assert.equal(await viewer.source, '/v/clip');
  assert.deepEqual(closed, ['late']);
});

test('cache service failure falls back to the existing playable URL', async () => {
  const sessions = new PlaybackSessions({
    create: async () => {throw new Error('offline');},
    touch: async () => {},
    close: async () => {},
  });
  const viewer = sessions.acquire('clip', '/v/clip');
  assert.equal(await viewer.source, '/v/clip');
  viewer.release();
});
