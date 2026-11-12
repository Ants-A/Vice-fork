import {useCallback, useEffect, useRef, useState} from 'react';
import {usePlayback} from '../state/playback';
import {ThumbnailRetry} from './thumbnailRetry';

/** Cached images may still load behind a player, but cold/failed thumbnails
 * retry only near the visible gallery and resume promptly after playback. */
export function useThumbnail(url: string | undefined, enabled = true) {
  const {thumbnailsPaused} = usePlayback();
  const containerRef = useRef<HTMLElement>(null);
  const controller = useRef<ThumbnailRetry | null>(null);
  const [visible, setVisible] = useState(false);
  const [suspended, setSuspended] = useState(thumbnailsPaused);
  const [available, setAvailable] = useState(() => document.visibilityState === 'visible' && navigator.onLine);
  const [image, setImage] = useState({source: url, src: url, failed: false, loaded: false});
  const [quality, setQuality] = useState<'low' | 'high'>('low');
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
    setQuality('low');
    setImage({source: url, src: url ? withWidth(url, 240) : url, failed: false, loaded: false});
  }, [url]);

  useEffect(() => {
    const retry = new ThumbnailRetry(request => {
      const base = withWidth(url, quality === 'low' ? 240 : 640);
      const separator = base?.includes('?') ? '&' : '?';
      setImage({source: url, src: `${base}${separator}retry=${request}`, failed: false, loaded: false});
    });
    controller.current = retry;
    return () => {
      retry.dispose();
      if (controller.current === retry) controller.current = null;
    };
  }, [url, quality]);

  useEffect(() => {
    controller.current?.setReady(Boolean(url) && enabled && visible && available && !blocked);
  }, [url, enabled, visible, available, blocked]);

  const onError = useCallback(() => {
    if (quality === 'high') {
      setQuality('low');
      setImage({source: url, src: withWidth(url, 240), failed: false, loaded: true});
      controller.current?.loaded();
      return;
    }
    setImage(previous => ({source: url, src: previous.source === url ? previous.src : url, failed: true, loaded: false}));
    controller.current?.error();
  }, [url, quality]);
  const onLoad = useCallback(() => {
    controller.current?.loaded();
    const connection = (navigator as Navigator & {connection?: {saveData?: boolean; effectiveType?: string}}).connection;
    if (quality === 'low' && !connection?.saveData && connection?.effectiveType !== '2g' && connection?.effectiveType !== 'slow-2g') {
      setQuality('high');
      setImage({source: url, src: withWidth(url, 640), failed: false, loaded: true});
    } else {
      setImage(previous => ({source: url, src: previous.source === url ? previous.src : withWidth(url, 240), failed: false, loaded: true}));
    }
  }, [url, quality]);
  const current = image.source === url ? image : {src: withWidth(url, 240), failed: false, loaded: false};
  // Removing an unfinished <img> cancels its HTTP request, freeing per-host
  // browser connections for the video. Finished images stay visible/cached.
  return {containerRef, src: current.loaded || !blocked ? current.src : undefined, loaded: current.loaded, failed: current.failed, onError, onLoad};
}

function withWidth(url: string | undefined, width: 240 | 640): string | undefined {
  if (!url) return undefined;
  try {
    const parsed = new URL(url, window.location.href);
    parsed.searchParams.set('w', String(width));
    return parsed.origin === window.location.origin ? `${parsed.pathname}${parsed.search}${parsed.hash}` : parsed.toString();
  } catch {
    return url;
  }
}
