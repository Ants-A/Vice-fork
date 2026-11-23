import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';

const values = new Map();
globalThis.localStorage = {getItem: key => values.get(key) ?? null, setItem: (key, value) => values.set(key, value)};
const events = {};
globalThis.window = {addEventListener: (name, fn) => { events[name] = fn; }};
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/favorites.ts', import.meta.url), 'utf8'))
  .replace("import {useSyncExternalStore} from 'react';", 'const useSyncExternalStore = (_subscribe, getSnapshot) => getSnapshot();');
const favorites = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);

test('clip and image favorites remain separate, persist, and follow renames and deletion', () => {
  favorites.toggleFavorite('clips', 'same');
  assert.deepEqual(favorites.useFavorites(), {clips: ['same'], images: []});
  favorites.toggleFavorite('images', 'same');
  favorites.moveFavorite('clips', 'same', 'renamed');
  assert.deepEqual(JSON.parse(values.get('vice-favorites')), {clips: ['renamed'], images: ['same']});
  favorites.moveFavorite('images', 'same', null);
  assert.deepEqual(favorites.useFavorites(), {clips: ['renamed'], images: []});
  favorites.toggleFavorite('clips', 'renamed');
  assert.deepEqual(favorites.useFavorites(), {clips: [], images: []});
});

test('storage changes in another window refresh the snapshot', () => {
  values.set('vice-favorites', JSON.stringify({clips: ['external'], images: ['picture']}));
  events.storage({key: 'vice-favorites'});
  assert.deepEqual(favorites.useFavorites(), {clips: ['external'], images: ['picture']});
});
