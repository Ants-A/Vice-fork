import {useState} from 'react';
import {IconPlaylist} from './Icons';
import {playlistIconSource, type PlaylistArtwork} from '../lib/playlistIcon';

export function PlaylistMark({playlist, size = 16}: {playlist: PlaylistArtwork; size?: number}) {
  const source = playlistIconSource(playlist);
  const [failedSource, setFailedSource] = useState<string | null>(null);
  return source && source !== failedSource ? (
    <img className="playlist-icon-image" src={source} width={size} height={size} alt="" aria-hidden="true"
      loading="lazy" decoding="async" onError={() => setFailedSource(source)} />
  ) : playlist.emoji ? <>{playlist.emoji}</> : <IconPlaylist size={size} />;
}
