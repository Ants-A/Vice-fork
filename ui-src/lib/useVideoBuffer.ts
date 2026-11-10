import {useEffect, useState, type RefObject} from 'react';
import {bufferSnapshot, type BufferedRange} from './mediaBuffer';

export function useVideoBuffer(
  ref: RefObject<HTMLVideoElement | null>, present: boolean, source: string, fallbackDuration: number,
) {
  const [state, setState] = useState({...bufferSnapshot([], fallbackDuration, 0), waiting: false});
  useEffect(() => {
    const video = ref.current;
    if (!present || !video) return;
    let waiting = video.readyState < 2 || (!video.paused && video.readyState < 3);
    const update = () => {
      const ranges: BufferedRange[] = [];
      for (let i = 0; i < video.buffered.length; i++) {
        ranges.push({start: video.buffered.start(i), end: video.buffered.end(i)});
      }
      const duration = Math.max(fallbackDuration, Number.isFinite(video.duration) ? video.duration : 0);
      setState({...bufferSnapshot(ranges, duration, video.currentTime), waiting});
    };
    const wait = () => {waiting = true; update();};
    const ready = () => {waiting = false; update();};
    const seek = () => {waiting = video.readyState < 2 || (!video.paused && video.readyState < 3); update();};
    const stalled = () => {if (!video.paused && video.readyState < 3) wait();};
    const pause = () => {if (video.readyState >= 2) ready();};
    const events: Array<[string, () => void]> = [
      ['loadstart', wait], ['waiting', wait], ['stalled', stalled], ['seeking', seek],
      ['playing', ready], ['canplay', ready], ['loadeddata', ready], ['seeked', seek],
      ['pause', pause], ['error', ready], ['ended', ready],
      ['progress', update], ['durationchange', update], ['loadedmetadata', update], ['timeupdate', update],
    ];
    events.forEach(([name, listener]) => video.addEventListener(name, listener));
    update();
    return () => events.forEach(([name, listener]) => video.removeEventListener(name, listener));
  }, [ref, present, source, fallbackDuration]);
  return state;
}
