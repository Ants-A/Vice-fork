/** Game labels are derived from automatic playlist membership. They are not
 * media metadata: a delayed probe/list response must not restore an old tag. */
interface GamePlaylist {
  kind: string;
  name: string;
  game: string | null;
  clip_slugs: string[];
  game_priority?: number;
  created_at?: string;
}
interface TaggedMedia {slug: string; game: string | null}
interface Candidate {game: string; priority?: number; created: string}
const indexes = new WeakMap<readonly GamePlaylist[], Map<string, Candidate[]>>();

function gameIndex(playlists: readonly GamePlaylist[]): Map<string, Candidate[]> {
  let index = indexes.get(playlists);
  if (index) return index;
  index = new Map();
  playlists.forEach(playlist => {
    if (playlist.kind !== 'auto') return;
    const candidate = {
      game: playlist.game || playlist.name,
      priority: playlist.game_priority,
      created: playlist.created_at ?? '',
    };
    for (const slug of playlist.clip_slugs) {
      const candidates = index!.get(slug);
      if (candidates) candidates.push(candidate);
      else index!.set(slug, [candidate]);
    }
  });
  indexes.set(playlists, index);
  return index;
}

function gameFor(candidates: Candidate[] | undefined, current: string | null): string | null {
  if (!candidates?.length) return null;
  if (candidates.length === 1) return candidates[0].game;
  const ranked = candidates.filter(candidate => candidate.priority !== undefined);
  if (ranked.length) {
    return ranked.reduce((first, candidate) => candidate.priority! < first.priority! ? candidate : first).game;
  }
  // Older daemons do not send the storage order. Keep their existing choice
  // when it is still valid, rather than changing it as sidebar counts change.
  if (current !== null && candidates.some(candidate => candidate.game === current)) return current;
  return candidates.reduce((first, candidate) =>
    candidate.created && first.created && candidate.created < first.created ? candidate : first).game;
}

/** Preserve row/list identities when tags have not changed so an icon change
 * or normal metadata update does not rebuild the whole gallery. */
export function applyPlaylistGames<T extends TaggedMedia>(
  rows: T[], playlists: readonly GamePlaylist[], image = false,
): T[] {
  const index = gameIndex(playlists);
  let result = rows;
  rows.forEach((row, position) => {
    const game = gameFor(index.get(image ? `img:${row.slug}` : row.slug), row.game);
    if (game === row.game) return;
    if (result === rows) result = rows.slice();
    result[position] = {...row, game};
  });
  return result;
}
