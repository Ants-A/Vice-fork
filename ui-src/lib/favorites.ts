import {useState} from 'react';
import {useStore} from '../state/store';
import {api} from './api';
import {t} from './i18n';
import type {Clip, Image} from './types';

export function useFavorites() {
  const {state} = useStore();
  return {clips: state.clips.filter(c => c.favorite).map(c => c.slug),
    images: state.images.filter(i => i.favorite).map(i => i.slug)};
}

export function useFavoriteToggle() {
  const {dispatch, notify} = useStore();
  const [pending, setPending] = useState(false);
  const toggle = async (kind: 'clips' | 'images', slug: string, favorite: boolean) => {
    if (pending) return;
    setPending(true);
    try {
      const item = await api.setFavorite(kind, slug, favorite);
      if (kind === 'clips') dispatch({type: 'ws', msg: {type: 'clip_saved', clip: item as Clip}});
      else dispatch({type: 'ws', msg: {type: 'image_saved', image: item as Image}});
    } catch (err) {
      notify({kind: 'error', title: t('favorites.saveFailed'), detail: (err as Error).message, tone: 'error', holdMs: 7000});
    } finally { setPending(false); }
  };
  return {toggle, pending};
}
