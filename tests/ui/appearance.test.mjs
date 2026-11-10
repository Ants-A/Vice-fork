import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {stripTypeScriptTypes} from 'node:module';
const load = async path => {
  const source = stripTypeScriptTypes(await readFile(new URL(path, import.meta.url), 'utf8'));
  return import(`data:text/javascript;base64,${Buffer.from(source).toString('base64')}`);
};
const {ACCENTS} = await load('../../ui-src/theme/accents.ts');
const {darkRamp} = await load('../../ui-src/theme/appearance.ts');
function luminance(hex) {
  const rgb = [1, 3, 5].map(offset => parseInt(hex.slice(offset, offset + 2), 16) / 255)
    .map(value => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
  return rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722;
}
function contrast(a, b) {
  const [low, high] = [luminance(a), luminance(b)].sort((a, b) => a - b);
  return (high + 0.05) / (low + 0.05);
}

test('dark surfaces are substantially darker for every shipped accent', () => {
  for (const light of Object.values(ACCENTS)) {
    const dark = darkRamp(light);
    for (const role of ['bg', 'surfaceLowest', 'surfaceLow', 'surface', 'surfaceHigh', 'surfaceHighest']) {
      assert(luminance(dark[role]) < luminance(light[role]) * 0.55, `${role} should be visibly darker`);
    }
    assert.equal(dark.base, light.base, 'accent selection remains independent of appearance');
    assert.equal(dark.ambient, 'transparent', 'ambient wash must not brighten the dark page');
  }
});
test('dark text and filled controls retain readable contrast', () => {
  for (const light of Object.values(ACCENTS)) {
    const dark = darkRamp(light);
    for (const role of ['bg', 'surfaceLow', 'surface', 'surfaceHigh', 'surfaceHighest']) {
      assert(contrast(dark.onSurface, dark[role]) >= 7, `primary text on ${role}`);
      assert(contrast(dark.onSurfaceVariant, dark[role]) >= 4.5, `secondary text on ${role}`);
    }
    for (const [fill, text] of [['base', 'onBase'], ['primaryContainer', 'onPrimaryContainer'], ['secondaryContainer', 'onSecondaryContainer'], ['errorContainer', 'onErrorContainer']]) {
      assert(contrast(dark[fill], dark[text]) >= 4.5, `${fill} must retain readable text`);
    }
  }
});
