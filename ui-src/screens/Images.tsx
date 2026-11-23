import {useFavorites} from '../lib/favorites';
import {useStore} from '../state/store';
import {useImageActions} from '../state/imageActions';
import {t} from '../lib/i18n';
import {hotkeyLabel} from '../lib/format';
import {ImageCard} from '../components/ImageCard';

/**
 * The clips grid, with pictures in it. Same header, same grid, same card, so
 * moving between the two sections changes only what is on the tiles.
 */
export function Images() {
  const {state, visibleImages: allVisibleImages} = useStore();
  const favorites = useFavorites();
  const favoriteView = state.view === 'favorite-images';
  const visibleImages = favoriteView ? allVisibleImages.filter(image => favorites.images.includes(image.slug)) : allVisibleImages;
  const {searchQuery, config} = state;

  const {actions, overlays} = useImageActions(favoriteView ? visibleImages : undefined);

  const count = visibleImages.length;
  const query = searchQuery.trim();
  // hotkeyLabel falls back to the clip key when nothing is bound, which is
  // the wrong sentence here: an unset screenshot key means there is no key.
  const bound = (config?.hotkeys?.screenshot as string | undefined) || '';

  return (
    <div className="clips">
      <header className="clips-head">
        <div className="clips-title">
          <h1>{t(favoriteView ? 'favorites.images' : 'images.heading')}</h1>
          <p>
            {query ? t('images.countMatches', {count, query}) : t('images.countImages', {count})}
          </p>
        </div>
      </header>

      {count === 0 ? (
        <p className="home-empty">
          {query
            ? t('images.emptySearch', {query})
            : favoriteView
              ? t('favorites.emptyImages')
              : bound
              ? t('images.emptyLibrary', {hotkey: hotkeyLabel(bound)})
              : t('images.emptyNoKey')}
        </p>
      ) : (
        <div className="clip-grid">
          {visibleImages.map(image => (
            <ImageCard key={image.slug} image={image} draggable actions={actions} />
          ))}
        </div>
      )}

      {overlays}
    </div>
  );
}
