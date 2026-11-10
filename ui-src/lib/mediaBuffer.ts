export interface BufferedRange {start: number; end: number}

/** Clamp and merge real media ranges; seeking can leave several disconnected spans. */
export function bufferSnapshot(ranges: BufferedRange[], duration: number, current: number) {
  const known = Number.isFinite(duration) && duration > 0;
  const merged: BufferedRange[] = [];
  for (const range of ranges
    .filter(r => Number.isFinite(r.start) && Number.isFinite(r.end))
    .map(r => ({start: Math.max(0, r.start), end: known ? Math.min(duration, r.end) : r.end}))
    .filter(r => r.end > r.start)
    .sort((a, b) => a.start - b.start)) {
    const last = merged[merged.length - 1];
    if (last && range.start <= last.end + 0.01) last.end = Math.max(last.end, range.end);
    else merged.push({...range});
  }
  const seconds = merged.reduce((total, r) => total + r.end - r.start, 0);
  const cursor = Number.isFinite(current) ? Math.max(0, current) : 0;
  const active = merged.find(r => cursor >= r.start - 0.05 && cursor <= r.end);
  return {
    ranges: merged,
    percent: known ? Math.min(100, seconds / duration * 100) : null,
    ahead: active ? Math.max(0, active.end - cursor) : 0,
  };
}
