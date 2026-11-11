import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/playlistGames.ts', import.meta.url), 'utf8'));
const {applyPlaylistGames} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
const game = (name, members, priority=0) => ({kind:'auto',name,game:name,clip_slugs:members,game_priority:priority});

test('adding and removing game membership immediately changes the card without metadata refresh', () => {
  const clip = {slug:'clip',name:'Clip.mp4',game:null,duration:20};
  const tagged = applyPlaylistGames([clip], [game('Cyberpunk 2077',['clip'])]);
  assert.equal(tagged[0].game, 'Cyberpunk 2077');
  assert.equal(tagged[0].duration, 20);
  assert.equal(clip.game, null, 'reconciliation does not mutate earlier snapshots');
  assert.equal(applyPlaylistGames(tagged, [game('Cyberpunk 2077',[])])[0].game, null);
  assert.equal(applyPlaylistGames(tagged, [])[0].game, null, 'deleting the playlist removes its tag');
});
test('late HTTP metadata and live clip updates cannot restore old game labels', () => {
  const authoritative = [game('New game',['clip'])];
  assert.equal(applyPlaylistGames([{slug:'clip',game:'Old game',duration:10}], authoritative)[0].game, 'New game');
  assert.equal(applyPlaylistGames([{slug:'clip',game:'Old game',duration:12}], authoritative)[0].game, 'New game');
  assert.equal(applyPlaylistGames([{slug:'clip',game:'Old game'}], [])[0].game, null);
});
test('game label follows backend game/name rule and custom playlists are not game tags', () => {
  const row = {slug:'clip',game:null};
  const custom = {...game('Favorites',['clip']),kind:'custom'};
  assert.equal(applyPlaylistGames([row],[custom])[0].game,null);
  assert.equal(applyPlaylistGames([row],[{...game('Display title',['clip']),game:'Actual game'}])[0].game,'Actual game');
  assert.equal(applyPlaylistGames([row],[{...game('Fallback name',['clip']),game:''}])[0].game,'Fallback name');
});
test('backend priority wins even when sidebar count sorting reverses game playlists', () => {
  const playlists = [game('Second game',['clip','another'],1),game('First game',['clip'],0)];
  assert.equal(applyPlaylistGames([{slug:'clip',game:'Second game'}],playlists)[0].game,'First game');
  assert.equal(applyPlaylistGames([{slug:'clip',game:'Second game'}],playlists.slice().reverse())[0].game,'First game');
  assert.equal(applyPlaylistGames([{slug:'clip',game:'First game'}],[playlists[0]])[0].game,'Second game');
});
test('images use prefixed membership without leaking a clip tag onto the same image slug', () => {
  const playlists = [game('Video game',['same'],0),game('Image game',['img:same'],1)];
  assert.equal(applyPlaylistGames([{slug:'same',game:null}],playlists)[0].game,'Video game');
  assert.equal(applyPlaylistGames([{slug:'same',game:null}],playlists,true)[0].game,'Image game');
});
test('unchanged tags preserve row and array identities during icon changes or normal refreshes', () => {
  const rows = [{slug:'clip',game:'Game'},{slug:'other',game:null}];
  assert.equal(applyPlaylistGames(rows,[game('Game',['clip'])]),rows);
  const updated = applyPlaylistGames(rows,[game('Changed game',['clip'])]);
  assert.notEqual(updated,rows);
  assert.equal(updated[1],rows[1]);
});
test('older daemons keep a valid game choice when multiple memberships lack priority hints', () => {
  const playlists = [game('B',['clip']),game('A',['clip'])].map(({game_priority,...playlist})=>playlist);
  assert.equal(applyPlaylistGames([{slug:'clip',game:'A'}],playlists)[0].game,'A');
});
