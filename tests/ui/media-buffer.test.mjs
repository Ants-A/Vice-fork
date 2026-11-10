import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const load = async path => {
  const source = stripTypeScriptTypes(await readFile(new URL(path, import.meta.url), 'utf8'));
  return import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
};
const {bufferSnapshot} = await load('../../ui-src/lib/mediaBuffer.ts');
const {supportsAnyVideo, previewFormat, preferSmallerPreview} = await load('../../ui-src/lib/mediaSupport.ts');

test('seeking counts disconnected ranges without pretending gaps are buffered', () => {
  const state = bufferSnapshot([{start: 0, end: 10}, {start: 40, end: 50}], 100, 45);
  assert.equal(state.percent, 20);
  assert.equal(state.ahead, 5);
  assert.equal(bufferSnapshot(state.ranges, 100, 20).ahead, 0);
});
test('overlapping ranges merge and clamp to duration', () => {
  assert.deepEqual(bufferSnapshot([{start: 5,end: 30},{start: -2,end: 10},{start: NaN,end: 1}],20,5), {
    ranges: [{start: 0,end: 20}], percent: 100,ahead: 15,
  });
});
test('unknown length stays indeterminate and never invents progress', () => {
  assert.equal(bufferSnapshot([], NaN, 0).percent, null);
  assert.equal(bufferSnapshot([{start: 0,end: 3}], Infinity, 1).ahead, 2);
  assert.equal(bufferSnapshot([], 60, 0).percent, 0);
});

test('engines without H264 can play a WebM preview', () => {
  assert.equal(previewFormat('h264', false, false, true), 'webm');
  assert.equal(previewFormat('hevc', true, false, true), 'mp4');
  assert.equal(previewFormat('h264', true, false, true), null);
  assert.equal(previewFormat('h264', true, false, true, true), 'mp4');
  assert.equal(previewFormat('hevc', true, true, true), null);
  assert.equal(previewFormat('h264', false, false, false), null);
});
test('codec support tries baseline and main rather than one high profile', () => {
  assert.equal(supportsAnyVideo(type => type === 'baseline' ? 'probably' : '', ['high', 'baseline']), true);
  assert.equal(supportsAnyVideo(() => '', ['high', 'baseline']), false);
});

test('large high-bitrate recordings default to a smaller preview', () => {
  assert.equal(preferSmallerPreview(340 * 1024 * 1024, 60), true);
  assert.equal(preferSmallerPreview(20 * 1024 * 1024, 60), false);
  assert.equal(preferSmallerPreview(340 * 1024 * 1024, 1200), false);
  assert.equal(preferSmallerPreview(340 * 1024 * 1024, null), false);
});
