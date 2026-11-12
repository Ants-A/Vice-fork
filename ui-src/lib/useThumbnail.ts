import {useCallback, useEffect, useRef, useState} from 'react';
import {usePlayback} from '../state/playback';
import {useThumbnailQuality} from './thumbnailQuality';
import {ThumbnailRetry} from './thumbnailRetry';

/** Cached images may still load behind a player, but cold/failed thumbnails
 * retry only near the visible gallery and resume promptly after playback. */
export function useThumbnail(url: string | undefined, enabled = true) {
  const {thumbnailsPaused} = usePlayback();
  const width = useThumbnailQuality();
  const containerRef = useRef<HTMLElement>(null);
  const controller = useRef<ThumbnailRetry | null>(null);
  const [visible, setVisible] = useState(false);
  const [suspended, setSuspended] = useState(thumbnailsPaused);
  const [available, setAvailable] = useState(() => document.visibilityState === 'visible' && navigator.onLine);
  const [image, setImage] = useState({source: url, src: url, failed: false, loaded: false});
  const blocked = thumbnailsPaused || suspended;

  useEffect(() => {
    if (thumbnailsPaused) {
      setSuspended(true);
      return;
    }
    if (!suspended) return;
    // Leave time for canceled video responses and the server's short playback
    // cooldown to release before attaching cold gallery requests again.
    const timer = window.setTimeout(() => setSuspended(false), 250);
    return () => window.clearTimeout(timer);
  }, [thumbnailsPaused, suspended]);

  useEffect(() => {
    const update = () => setAvailable(document.visibilityState === 'visible' && navigator.onLine);
    document.addEventListener('visibilitychange', update);
    window.addEventListener('online', update);
    window.addEventListener('offline', update);
    return () => {
      document.removeEventListener('visibilitychange', update);
      window.removeEventListener('online', update);
      window.removeEventListener('offline', update);
    };
  }, []);

  useEffect(() => {
    const node = containerRef.current;
    if (!node) return;
    const observer = new IntersectionObserver(entries => {
      setVisible(entries.some(entry => entry.isIntersecting && entry.target.getClientRects().length > 0));
    }, {root: node.closest('.frame-content'), rootMargin: '200px 0px'});
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    setImage({source: url, src: withWidth(url, width), failed: false, loaded: false});
  }, [url, width]);

  useEffect(() => {
    const retry = new ThumbnailRetry(request => {
      const base = withWidth(url, width);
      const separator = base?.includes('?') ? '&' : '?';
      setImage({source: url, src: `${base}${separator}retry=${request}`, failed: false, loaded: false});
    });
    controller.current = retry;
    return () => {
      retry.dispose();
      if (controller.current === retry) controller.current = null;
    };
  }, [url, width]);

  useEffect(() => {
    controller.current?.setReady(Boolean(url) && enabled && visible && available && !blocked);
  }, [url, width, enabled, visible, available, blocked]);

  const onError = useCallback(() => {
    setImage(previous => ({source: url, src: previous.source === url ? previous.src : withWidth(url, width), failed: true, loaded: false}));
    controller.current?.error();
  }, [url, width]);
  const onLoad = useCallback(() => {
    controller.current?.loaded();
    setImage(previous => ({source: url, src: previous.source === url ? previous.src : withWidth(url, width), failed: false, loaded: true}));
  }, [url, width]);
  const current = image.source === url ? image : {src: withWidth(url, width), failed: false, loaded: false};
  // Removing an unfinished <img> cancels its HTTP request, freeing per-host
  // browser connections for the video. Finished images stay visible/cached.
  return {containerRef, src: current.loaded || !blocked ? current.src : undefined, loaded: current.loaded, failed: current.failed, onError, onLoad};
}

function withWidth(url: string | undefined, width: number): string | undefined {
  if (!url) return undefined;
  try {
    const parsed = new URL(url, window.location.href);
    if (width === 160) parsed.searchParams.set('w', '160');
    else parsed.searchParams.delete('w');
    return parsed.origin === window.location.origin ? `${parsed.pathname}${parsed.search}${parsed.hash}` : parsed.toString();
  } catch {
    return url;
  }
}
