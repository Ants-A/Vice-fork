import {useEffect, useState} from 'react';

import {api} from './api';
import {PlaybackSessions} from './playbackSessions';

const sessions = new PlaybackSessions({
  create: api.createPlaybackSession,
  touch: api.touchPlaybackSession,
  close: api.closePlaybackSession,
});

/** Retain downloaded ranges for the entire open player, including after EOS. */
export function usePlaybackSession(slug: string | undefined, source: string, open: boolean): string {
  const [ready, setReady] = useState<{source: string; url: string} | null>(null);
  useEffect(() => {
    if (!open || !slug || !source) return;
    let active = true;
    const lease = sessions.acquire(slug, source);
    // Session creation is an optional optimisation; a stalled request must not
    // prevent the player from attempting the original URL.
    const fallback = window.setTimeout(() => {
      if (active) setReady({source, url: source});
    }, 5000);
    void lease.source.then(url => {
      window.clearTimeout(fallback);
      if (active) setReady(previous => previous?.source === source ? previous : {source, url});
    });
    const release = () => {
      active = false;
      window.clearTimeout(fallback);
      lease.release();
    };
    window.addEventListener('pagehide', release);
    return () => {
      window.removeEventListener('pagehide', release);
      release();
      setReady(null);
    };
  }, [slug, source, open]);
  return open && ready?.source === source ? ready.url : '';
}
