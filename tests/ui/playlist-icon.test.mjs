import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const source = stripTypeScriptTypes(await readFile(new URL('../../ui-src/lib/playlistIcon.ts', import.meta.url), 'utf8'));
const {playlistIconSource} = await import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
const game = {kind:'auto',name:'Game',game:'Game & Extra?',emoji:''};
test('automatic game playlists request artwork without delaying the list', () => {
  assert.equal(playlistIconSource(game), '/api/game-icon?name=Game%20%26%20Extra%3F');
  assert.equal(playlistIconSource({...game, icon:'auto', emoji:'🎮'}), '/api/game-icon?name=Game%20%26%20Extra%3F');
});
test('explicit emoji mode disables automatic artwork and uploads take precedence', () => {
  assert.equal(playlistIconSource({...game, icon:''}), null);
  assert.equal(playlistIconSource({...game, kind:'custom'}), null);
  const icon='data:image/png;base64,uploaded';
  assert.equal(playlistIconSource({...game, icon}), icon);
});
test('playlist artwork never renders arbitrary URLs or executable image formats', () => {
  for (const icon of ['javascript:bad', 'data:image/svg+xml,<svg/>', 'https://example.com/image', 'data:image/png;base64,'+'a'.repeat(65536)]) {
    assert.equal(playlistIconSource({...game, icon}), null);
  }
});
