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
    const retry = new ThumbnailRetry(request => {
      const separator = url?.includes('?') ? '&' : '?';
      setImage({source: url, src: `${url}${separator}retry=${request}`, failed: false, loaded: false});
    });
    controller.current = retry;
    return () => {
      retry.dispose();
      if (controller.current === retry) controller.current = null;
    };
  }, [url]);

  useEffect(() => {
    controller.current?.setReady(Boolean(url) && enabled && visible && available && !blocked);
  }, [url, enabled, visible, available, blocked]);

  const onError = useCallback(() => {
    setImage(previous => ({source: url, src: previous.source === url ? previous.src : url, failed: true, loaded: false}));
    controller.current?.error();
  }, [url]);
  const onLoad = useCallback(() => {
    controller.current?.loaded();
    setImage(previous => ({source: url, src: previous.source === url ? previous.src : url, failed: false, loaded: true}));
  }, [url]);
  const current = image.source === url ? image : {src: url, failed: false, loaded: false};
  // Removing an unfinished <img> cancels its HTTP request, freeing per-host
  // browser connections for the video. Finished images stay visible/cached.
  return {containerRef, src: current.loaded || !blocked ? current.src : undefined, loaded: current.loaded, failed: current.failed, onError, onLoad};
}
