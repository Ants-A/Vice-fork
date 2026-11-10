export function supportsAnyVideo(probe: (type: string) => string, types: string[]): boolean {
  try {return types.some(type => /^(maybe|probably)$/.test(probe(type)));}
  catch {return false;}
}

export function previewFormat(codec: string, h264: boolean, hevc: boolean, webm: boolean, smaller = false): 'mp4' | 'webm' | null {
  if (smaller && h264) return 'mp4';
  if (!h264 && webm) return 'webm';
  return /^(hevc|h265)$/i.test(codec) && !hevc && h264 ? 'mp4' : null;
}

/** Large recording bitrates can exceed the mounted share's read throughput. */
export function preferSmallerPreview(size: number | null | undefined, duration: number | null | undefined): boolean {
  return Boolean(size && duration && duration > 0 && size > 64 * 1024 * 1024 && size * 8 / duration > 12_000_000);
}
