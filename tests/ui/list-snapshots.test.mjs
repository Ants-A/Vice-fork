import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/listSnapshots.ts', import.meta.url), 'utf8'));
const {ListSnapshots} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('a clip saved during a refresh survives the older HTTP list', () => {
  const lists = new ListSnapshots();
  const mark = lists.begin();
  lists.observe('new', {slug: 'new', duration: 12});
  assert.deepEqual(lists.finish(mark, [{slug: 'old'}]), [{slug: 'new', duration: 12}, {slug: 'old'}]);
});
test('refreshes preserve live renames, completed metadata and deletions', () => {
  const lists = new ListSnapshots();
  const mark = lists.begin();
  lists.observe('clip', {slug: 'clip', name: 'new name', duration: 9});
  lists.observe('deleted', null);
  assert.deepEqual(lists.finish(mark, [{slug: 'clip', name: 'old', duration: null, size: 55}, {slug: 'deleted'}]),
    [{slug: 'clip', name: 'new name', duration: 9, size: 55}]);
});
test('an earlier request cannot overwrite a newer completed refresh', () => {
  const lists = new ListSnapshots();
  const old = lists.begin();
  const recent = lists.begin();
  assert.deepEqual(lists.finish(recent, [{slug: 'new'}]), [{slug: 'new'}]);
  assert.equal(lists.finish(old, [{slug: 'old'}]), null);
});
test('failed overlapping requests retain changes needed by remaining requests', () => {
  const lists = new ListSnapshots();
  const first = lists.begin();
  const failed = lists.begin();
  lists.observe('live', {slug: 'live'});
  assert.equal(lists.finish(failed, null), null);
  assert.deepEqual(lists.finish(first, []), [{slug: 'live'}]);
  const next = lists.begin();
  assert.deepEqual(lists.finish(next, []), []);
});
