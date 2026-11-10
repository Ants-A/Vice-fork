import {useEffect, useRef, useState} from 'react';

import {Modal} from './Modal';
import {PlaylistMark} from './PlaylistMark';
import {readPlaylistIcon} from '../lib/playlistIcon';
import type {Playlist} from '../lib/types';
import {t} from '../lib/i18n';

/** The gradient pairs a playlist can carry. Order matters: index is stored. */
export const PLAYLIST_COLORS: [string, string][] = [
  ['#ff7a45', '#9a3412'],
  ['#f0b429', '#7c4a03'],
  ['#34d399', '#064e3b'],
  ['#38bdf8', '#075985'],
  ['#8b5cf6', '#3b0a74'],
  ['#f472b6', '#831843'],
  ['#ef4444', '#7f1d1d'],
  ['#a3e635', '#3f6212'],
];

const DEFAULT_COLOR = 4;

export interface PlaylistDraft {
  name: string;
  emoji: string;
  color1: string;
  color2: string;
  icon: string;
}

export function PlaylistModal({
  open,
  editing,
  onClose,
  onSubmit,
}: {
  open: boolean;
  /** The playlist being edited, or null when creating a new one. */
  editing: Playlist | null;
  onClose: () => void;
  onSubmit: (draft: PlaylistDraft) => Promise<void>;
}) {
  const [name, setName] = useState('');
  const [emoji, setEmoji] = useState('');
  const [icon, setIcon] = useState('');
  const [readingIcon, setReadingIcon] = useState(false);
  const iconInput = useRef<HTMLInputElement>(null);
  const [colorIndex, setColorIndex] = useState(DEFAULT_COLOR);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setSaving(false);
    if (editing) {
      setName(editing.name);
      setEmoji(editing.emoji ?? '');
      setIcon(editing.icon ?? (editing.kind === 'auto' && !editing.emoji ? 'auto' : ''));
      const found = PLAYLIST_COLORS.findIndex(
        ([a, b]) => a === editing.color1 && b === editing.color2,
      );
      setColorIndex(found >= 0 ? found : DEFAULT_COLOR);
    } else {
      setName('');
      setEmoji('');
      setIcon('');
      setColorIndex(DEFAULT_COLOR);
    }
  }, [open, editing]);

  const [c1, c2] = PLAYLIST_COLORS[colorIndex];

  const submit = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError(t('playlistModal.nameRequired'));
      return;
    }
    setSaving(true);
    try {
      await onSubmit({name: trimmed, emoji: emoji.trim(), color1: c1, color2: c2, icon});
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={open}
      title={editing ? t('playlistModal.editTitle') : t('playlistModal.newTitle')}
      onClose={onClose}
      footer={
        <>
          <button type="button" className="btn btn-quiet" onClick={onClose}>
            {t('common.cancel')}
          </button>
          <button type="button" className="btn" onClick={() => void submit()} disabled={saving || readingIcon}>
            {editing ? t('common.save') : t('common.create')}
          </button>
        </>
      }>
      <div className="npl-head">
        <span
          className="npl-preview"
          style={{background: `linear-gradient(150deg, ${c1}, ${c2})`}}
          aria-hidden="true">
          <PlaylistMark playlist={{kind: editing?.kind ?? 'custom', game: editing?.game ?? null, name, emoji, icon}} size={44} />
        </span>
        <div className="npl-fields">
          <label className="field">
            <span>{t('playlistModal.name')}</span>
            <input
              value={name}
              autoFocus
              onChange={e => {
                setName(e.target.value);
                setError(null);
              }}
              onKeyDown={e => e.key === 'Enter' && void submit()}
            />
          </label>
          <label className="field field-narrow">
            <span>{t('playlistModal.emoji')}</span>
            <input
              value={emoji}
              maxLength={4}
              placeholder={t('playlistModal.emojiPlaceholder')}
              onChange={e => {setEmoji(e.target.value); setIcon('');}}
            />
          </label>
        </div>
      </div>

      <div className="npl-icon-options">
        <span>{t('playlistModal.icon')}</span>
        {editing?.kind === 'auto' ? (
          <button type="button" className="btn btn-quiet" aria-pressed={icon === 'auto'}
            onClick={() => setIcon('auto')}>{t('playlistModal.gameIcon')}</button>
        ) : null}
        <button type="button" className="btn btn-quiet" aria-pressed={icon.startsWith('data:')}
          disabled={readingIcon} onClick={() => iconInput.current?.click()}>{t('playlistModal.chooseImage')}</button>
        <button type="button" className="btn btn-quiet" aria-pressed={!icon}
          onClick={() => setIcon('')}>{t('playlistModal.useEmoji')}</button>
        <input ref={iconInput} type="file" accept="image/png,image/jpeg,image/webp" hidden
          aria-label={t('playlistModal.chooseImage')}
          onChange={async event => {
            const file = event.currentTarget.files?.[0];
            event.currentTarget.value = '';
            if (!file) return;
            setReadingIcon(true); setError(null);
            try {setIcon(await readPlaylistIcon(file));}
            catch (err) {setError((err as Error).message);}
            finally {setReadingIcon(false);}
          }} />
      </div>

      <div className="npl-colors" role="radiogroup" aria-label={t('playlistModal.colour')}>
        {PLAYLIST_COLORS.map(([a, b], i) => (
          <button
            key={a}
            type="button"
            role="radio"
            aria-checked={i === colorIndex}
            aria-label={`Colour ${i + 1}`}
            className="npl-color"
            data-active={i === colorIndex || undefined}
            style={{background: `linear-gradient(150deg, ${a}, ${b})`}}
            onClick={() => setColorIndex(i)}
          />
        ))}
      </div>

      {error ? <p className="field-error">{error}</p> : null}
    </Modal>
  );
}
