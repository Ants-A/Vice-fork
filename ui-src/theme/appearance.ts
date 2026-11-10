import type {AccentRamp} from './accents';

export type Appearance = 'light' | 'dark';

/** Light deliberately preserves the palette Vice shipped before this option. */
export function appearance(value: unknown): Appearance {
  return value === 'dark' ? 'dark' : 'light';
}

function dim(hex: string, amount: number): string {
  return `#${[1, 3, 5].map(offset =>
    Math.round(parseInt(hex.slice(offset, offset + 2), 16) * amount).toString(16).padStart(2, '0'),
  ).join('')}`;
}

/** Near-black surfaces without dimming text, focus rings or accent buttons. */
export function darkRamp(a: AccentRamp): AccentRamp {
  return {
    ...a,
    bg: '#040506',
    surfaceLowest: '#020304',
    surfaceLow: '#090b0f',
    surface: '#0d0f14',
    surfaceHigh: '#14171d',
    surfaceHighest: '#1c2027',
    primaryContainer: dim(a.primaryContainer, 0.4),
    secondaryContainer: dim(a.secondaryContainer, 0.4),
    tertiaryContainer: dim(a.tertiaryContainer, 0.4),
    outlineVariant: dim(a.outlineVariant, 0.7),
    errorContainer: dim(a.errorContainer, 0.6),
    ambient: 'transparent',
  };
}
