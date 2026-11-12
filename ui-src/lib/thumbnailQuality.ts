import {useSyncExternalStore} from 'react';

type Quality = 'low' | 'high';
let quality: Quality = 'high';
const listeners = new Set<() => void>();

export function setThumbnailQuality(value: unknown): void {
  const next: Quality = value === 'low' ? 'low' : 'high';
  if (next === quality) return;
  quality = next;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useThumbnailQuality(): 160 | 320 {
  return useSyncExternalStore(subscribe, () => quality === 'low' ? 160 : 320, () => 320);
}
