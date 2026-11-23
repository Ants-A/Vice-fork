import {useSyncExternalStore} from 'react';

type Kind = 'clips' | 'images';
type Favorites = Record<Kind, string[]>;
const key = 'vice-favorites';
const empty: Favorites = {clips: [], images: []};
function read(): Favorites {
  try {
    const value = JSON.parse(localStorage.getItem(key) || '{}');
    return {
      clips: Array.isArray(value.clips) ? value.clips.filter((v: unknown) => typeof v === 'string') : [],
      images: Array.isArray(value.images) ? value.images.filter((v: unknown) => typeof v === 'string') : [],
    };
  } catch { return empty; }
}
let current = read();
const listeners = new Set<() => void>();
function publish(next: Favorites) {
  localStorage.setItem(key, JSON.stringify(next));
  current = next;
  listeners.forEach(fn => fn());
}
window.addEventListener('storage', event => {
  if (event.key === key || event.key === null) {
    current = read();
    listeners.forEach(fn => fn());
  }
});
export function useFavorites() {
  return useSyncExternalStore(fn => { listeners.add(fn); return () => { listeners.delete(fn); }; }, () => current);
}
export function toggleFavorite(kind: Kind, slug: string) {
  const items = current[kind];
  publish({...current, [kind]: items.includes(slug) ? items.filter(item => item !== slug) : [...items, slug]});
}
export function moveFavorite(kind: Kind, from: string, to: string | null) {
  if (!current[kind].includes(from)) return;
  publish({...current, [kind]: [...new Set(current[kind].filter(item => item !== from).concat(to ? [to] : []))]});
}
