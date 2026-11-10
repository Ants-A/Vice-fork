import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/hoverPreview.ts', import.meta.url), 'utf8'));
const {startHoverPreview, stopHoverPreview} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
const video = () => ({
  src: '', isConnected: true, paused: true, loads: 0,
  play() {this.paused = false; return Promise.resolve();},
  pause() {this.paused = true;},
  removeAttribute() {this.src = '';},
  getAttribute() {return this.src;},
  load() {this.loads++;},
});
test('sweeping across cards does not download each clip', t => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const first = video(), second = video();
  startHoverPreview(first, '/first.mp4', () => assert.fail());
  t.mock.timers.tick(100);
  startHoverPreview(second, '/second.mp4', () => assert.fail());
  t.mock.timers.tick(400);
  assert.equal(first.src, '');
  assert.equal(second.src, '/second.mp4');
  stopHoverPreview(second);
  assert.equal(second.src, '');
  assert.equal(second.paused, true);
  assert.equal(second.loads, 1, 'release must abort the network fetch as well as pause decoding');
});
test('opening the viewer stops a playing or pending hover download', t => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const first = video(), second = video();
  startHoverPreview(first, '/first.mp4', () => assert.fail());
  t.mock.timers.tick(400);
  startHoverPreview(second, '/second.mp4', () => assert.fail());
  assert.equal(first.src, '');
  stopHoverPreview();
  t.mock.timers.tick(500);
  assert.equal(second.src, '');
});
