import type {Playlist} from './types';

export type PlaylistArtwork = Pick<Playlist, 'kind' | 'game' | 'name' | 'emoji' | 'icon'>;
export function playlistIconSource(playlist: PlaylistArtwork): string | null {
  if (playlist.icon?.startsWith('data:image/png;base64,') && playlist.icon.length <= 65536) return playlist.icon;
  if (playlist.icon === 'auto' || (playlist.icon === undefined && playlist.kind === 'auto' && !playlist.emoji)) {
    const game = playlist.game || playlist.name;
    return game ? `/api/game-icon?name=${encodeURIComponent(game)}` : null;
  }
  return null;
}

/** Shrink a chosen raster image before storing it with the playlist. */
export async function readPlaylistIcon(file: File): Promise<string> {
  if (!['image/png', 'image/jpeg', 'image/webp'].includes(file.type) || file.size > 10 * 1024 * 1024) {
    throw new Error('Choose a PNG, JPEG or WebP image smaller than 10 MB.');
  }
  const url = URL.createObjectURL(file);
  try {
    const image = new Image();
    image.src = url;
    await new Promise<void>((resolve, reject) => {
      image.onload = () => resolve();
      image.onerror = () => reject(new Error('Could not read this image.'));
    });
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 96;
    const context = canvas.getContext('2d');
    if (!context) throw new Error('Image editing is unavailable.');
    const side = Math.min(image.naturalWidth, image.naturalHeight);
    context.drawImage(image, (image.naturalWidth-side)/2, (image.naturalHeight-side)/2, side, side, 0, 0, 96, 96);
    const icon = canvas.toDataURL('image/png');
    if (icon.length > 65536) throw new Error('This image could not be made small enough.');
    return icon;
  } finally { URL.revokeObjectURL(url); }
}
