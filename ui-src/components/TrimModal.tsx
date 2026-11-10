import {useEffect, useRef, useState} from 'react';

import {api} from '../lib/api';
import {useVideoBuffer} from '../lib/useVideoBuffer';
import {onWsMessage} from '../lib/ws';
import {useEscape} from '../lib/escape';
import {formatDuration} from '../lib/format';
import {
  clipNeedsProxy,
  playQuietly,
  playbackUrl,
  timecode,
  useVideoFailure,
  videoFailureMessage,
} from '../lib/playback';
import {clipTitle, type Clip, type Highlight} from '../lib/types';
import {IconClose} from './Icons';
import {InlineRename} from './InlineRename';
import {t} from '../lib/i18n';

/** The shortest selection a handle can leave behind. */
const MIN_SELECTION = 0.5;

/**
 * Trim a clip down to a selection.
 *
 * Playback goes through the H.264 proxy for codecs this window cannot decode,
 * so the selection can be scrubbed; the cut itself always runs on the original
 * file, so proxying never touches what lands on disk.
 */
export function TrimModal({
  clip,
  highlights,
  onClose,
  onSaved,
  notify,
  onRename,
  onReveal,
  onOpenExternally,
}: {
  clip: Clip | null;
  /** Shown on the timeline when they belong to this clip. */
  highlights: Highlight[];
  onClose: () => void;
  onSaved: () => Promise<void> | void;
  notify: (title: string, detail?: string, tone?: 'accent' | 'error') => void;
  onRename: (clip: Clip, name: string) => void;
  onReveal: (clip: Clip) => void;
  onOpenExternally: (clip: Clip) => void;
}) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const timelineRef = useRef<HTMLDivElement>(null);
  const [total, setTotal] = useState(0);
  const [selection, setSelection] = useState({start: 0, end: 0});
  const [playhead, setPlayhead] = useState(0);
  const [previewing, setPreviewing] = useState(false);
  const [preparing, setPreparing] = useState(false);
  const [previewProgress, setPreviewProgress] = useState<number | null>(null);
  const source = playbackUrl(clip);
  const buffer = useVideoBuffer(videoRef, clip !== null, source, clip?.duration ?? 0);
  useEffect(() => {setPreviewProgress(null);}, [source]);
  useEffect(() => onWsMessage(msg => {
    if (msg.type === 'preview_progress' && msg.slug === clip?.slug && Number.isFinite(msg.progress)) {
      setPreviewProgress(Math.max(0, Math.min(1, msg.progress)));
    }
  }), [clip?.slug]);
  const [dragging, setDragging] = useState<'start' | 'end' | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [renamingTitle, setRenamingTitle] = useState(false);
  // Held until the dialog closes. Renaming the file straight away swapped the
  // clip under the dialog, which threw the selection away and, for an H.265
  // clip, made the preview transcode all over again (#226).
  const [pendingName, setPendingName] = useState<string | null>(null);
  // Replacing the original asks twice, because it throws away everything
  // outside the selection (#255).
  const [confirmReplace, setConfirmReplace] = useState(false);
  const failed = useVideoFailure(videoRef, clip !== null);

  // Preview state is read inside a media event handler, so it needs a ref as
  // well: the listener would otherwise close over whatever it was at mount.
  const previewingRef = useRef(false);
  previewingRef.current = previewing;
  const selectionRef = useRef(selection);
  selectionRef.current = selection;

  const close = () => {
    if (clip && pendingName && pendingName !== clipTitle(clip)) onRename(clip, pendingName);
    setPendingName(null);
    onClose();
  };

  useEscape(clip !== null, close);

  useEffect(() => {
    const video = videoRef.current;
    if (!video || !clip) return;
    setTotal(clip.duration ?? 0);
    setSelection({start: 0, end: clip.duration ?? 0});
    setPlayhead(0);
    setPreviewing(false);
    setSaving(false);
    setError(null);
    setPreparing(clipNeedsProxy(clip));
    setPendingName(null);
    setConfirmReplace(false);
    video.src = playbackUrl(clip);
    video.load();
    return () => {
      video.pause();
      video.removeAttribute('src');
      video.load();
    };
  }, [clip?.slug, source]);

  if (!clip) return null;

  const stopPreview = () => {
    setPreviewing(false);
    previewingRef.current = false;
    videoRef.current?.pause();
  };

  const togglePreview = () => {
    const video = videoRef.current;
    if (!total || !video) return;
    if (previewing) {
      stopPreview();
      return;
    }
    setPreviewing(true);
    previewingRef.current = true;
    video.currentTime = selection.start;
    playQuietly(video);
  };

  const onTimeUpdate = () => {
    const video = videoRef.current;
    if (!video) return;
    setPlayhead(video.currentTime);
    if (!previewingRef.current || !total) return;
    const {start, end} = selectionRef.current;
    // Loop the selection. The tolerance on the lower bound is deliberate: a
    // scrub lands a little before the in point, and a tight comparison would
    // read every ordinary frame near the start as a scrub and stutter.
    if (video.currentTime >= end - 0.05 || video.currentTime < start - 0.3) {
      video.currentTime = start;
      if (video.paused) playQuietly(video);
    }
  };

  const beginDrag = (handle: 'start' | 'end') => (event: React.PointerEvent) => {
    if (event.button !== 0 || !total) return;
    event.preventDefault();
    setDragging(handle);
    if (previewingRef.current) stopPreview();

    const apply = (clientX: number) => {
      const rect = timelineRef.current?.getBoundingClientRect();
      const video = videoRef.current;
      if (!rect || !video) return;
      const at = (Math.max(0, Math.min(rect.width, clientX - rect.left)) / rect.width) * total;
      setSelection(prev => {
        const next =
          handle === 'start'
            ? {...prev, start: Math.max(0, Math.min(at, prev.end - MIN_SELECTION))}
            : {...prev, end: Math.min(total, Math.max(at, prev.start + MIN_SELECTION))};
        selectionRef.current = next;
        video.currentTime = handle === 'start' ? next.start : next.end;
        return next;
      });
    };

    const move = (e: PointerEvent) => apply(e.clientX);
    const up = () => {
      window.removeEventListener('pointermove', move, true);
      window.removeEventListener('pointerup', up, true);
      window.removeEventListener('pointercancel', up, true);
      setDragging(null);
    };
    apply(event.clientX);
    window.addEventListener('pointermove', move, true);
    window.addEventListener('pointerup', up, true);
    window.addEventListener('pointercancel', up, true);
  };

  const save = async (copy: boolean) => {
    if (previewing) stopPreview();
    setSaving(true);
    setError(null);
    setConfirmReplace(false);
    try {
      const result = await api.trimClip(clip.slug, selection.start, selection.end, copy);
      const name = pendingName && pendingName !== clipTitle(clip) ? pendingName : null;
      setPendingName(null);
      // A name typed while saving a copy belongs to the copy: the original is
      // the clip being kept as it was.
      if (name) onRename(copy && result.slug ? {...clip, slug: result.slug} : clip, name);
      notify(copy ? t('trim.savedCopy') : t('trim.saved'), name ?? clipTitle(clip), 'accent');
      onClose();
      await onSaved();
    } catch (err) {
      const message = (err as Error).message || t('trim.failed');
      setError(message);
      notify(t('trim.errTrim'), message, 'error');
      setSaving(false);
    }
  };

  const pct = (seconds: number) => (total ? (seconds / total) * 100 : 0);
  const startPct = pct(selection.start);
  const endPct = pct(selection.end);

  return (
    <div className="scrim" onMouseDown={e => e.target === e.currentTarget && close()}>
      <div
        className="modal trim-modal"
        role="dialog"
        aria-modal="true"
        aria-label={t('trim.trimAria', {name: clipTitle(clip)})}>
        <div className="modal-head">
          {/* The name is its own element rather than part of a templated
              sentence, so it can be renamed from here without the field
              swallowing the word "Trim" as well (#170). */}
          <h2 className="trim-head">
            <span className="trim-head-label">{t('trim.heading')}</span>
            {renamingTitle ? (
              <InlineRename
                className="trim-rename"
                label={t('card.nameLabel')}
                initial={pendingName ?? clipTitle(clip)}
                onCancel={() => setRenamingTitle(false)}
                onSubmit={name => {
                  setRenamingTitle(false);
                  setPendingName(name);
                }}
              />
            ) : (
              <span
                className="trim-head-name"
                title={t('viewer.doubleClickRename')}
                onDoubleClick={() => setRenamingTitle(true)}>
                {pendingName ?? clipTitle(clip)}
              </span>
            )}
          </h2>
          <button type="button" className="modal-close" onClick={close} aria-label={t('common.close')}>
            <IconClose size={15} />
          </button>
        </div>

        <div className="modal-body">
          <div className="trim-stage">
            <video
              ref={videoRef}
              className="trim-video"
              controls
              playsInline
              onLoadedMetadata={e => {
                const seconds = Number.isFinite(e.currentTarget.duration)
                  ? e.currentTarget.duration
                  : 0;
                setTotal(seconds);
                const next = {start: 0, end: seconds};
                setSelection(next);
                selectionRef.current = next;
                setPreparing(false);
              }}
              onTimeUpdate={onTimeUpdate}
              onEnded={() => {
                const video = videoRef.current;
                if (!previewingRef.current || !video) return;
                video.currentTime = selectionRef.current.start;
                playQuietly(video);
              }}
              onCanPlay={() => setPreparing(false)}
              onError={() => setPreparing(false)}
            />
            {!failed && (preparing || buffer.waiting) ? (
              <div className="video-overlay video-buffering" role="status" aria-live="polite">
                <p>{t(preparing ? 'trim.preparingPreview' : 'viewer.buffering')}</p>
                <progress className="video-buffer-progress" aria-label={t('viewer.buffering')} max={100}
                  value={preparing ? previewProgress === null ? undefined : previewProgress * 100 : buffer.percent || undefined} />
                <span className="video-buffer-caption mono">{preparing && previewProgress !== null ? `${Math.floor(previewProgress * 100)}%` : !preparing && buffer.percent ? t('viewer.bufferedPercent', {percent: Math.floor(buffer.percent)}) : t('viewer.connecting')}</span>
              </div>
            ) : null}
            {failed ? (
              <div className="video-overlay">
                <p>{videoFailureMessage()}</p>
                <div className="video-overlay-actions">
                  <button type="button" className="btn" onClick={() => onOpenExternally(clip)}>
                    {t('trim.openInPlayer')}
                  </button>
                  <button type="button" className="btn btn-quiet" onClick={() => onReveal(clip)}>
                    {t('trim.showInFolder')}
                  </button>
                </div>
              </div>
            ) : null}
          </div>

          <div className="trim-readout">
            <span>
              {t('trim.in')} <b className="mono">{formatDuration(selection.start, true)}</b>
            </span>
            <span>
              {t('trim.duration')}{' '}
              <b className="mono trim-readout-dur">
                {formatDuration(Math.max(0, selection.end - selection.start), true)}
              </b>
            </span>
            <span>
              {t('trim.out')} <b className="mono">{formatDuration(selection.end, true)}</b>
            </span>
          </div>

          <div className="trim-timeline" ref={timelineRef}>
            <div className="trim-track" />
            <div
              className="trim-selection"
              style={{left: `${startPct}%`, width: `${endPct - startPct}%`}}
            />
            {highlights.map(h => (
              <div
                key={h.id}
                className="trim-hl"
                style={{left: `${pct(h.time)}%`, background: h.color || '#f59e0b'}}
                title={h.label}
              />
            ))}
            <div className="trim-playhead" style={{left: `${pct(playhead)}%`}} />
            <button
              type="button"
              className="trim-handle"
              data-dragging={dragging === 'start' || undefined}
              style={{left: `${startPct}%`}}
              aria-label={t('trim.startOfSelection')}
              onPointerDown={beginDrag('start')}
            />
            <button
              type="button"
              className="trim-handle"
              data-dragging={dragging === 'end' || undefined}
              style={{left: `${endPct}%`}}
              aria-label={t('trim.endOfSelection')}
              onPointerDown={beginDrag('end')}
            />
          </div>

          <div className="trim-tc mono">
            <span>start {timecode(selection.start)}</span>
            <span>end {timecode(selection.end)}</span>
          </div>
        </div>

        <div className="modal-foot trim-foot">
          <button
            type="button"
            className={previewing ? 'btn btn-sm' : 'btn btn-quiet btn-sm'}
            onClick={togglePreview}
            disabled={!total}>
            {previewing ? t('trim.stop') : t('trim.preview')}
          </button>
          <span className="trim-status" data-warning={(!error && confirmReplace) || undefined}>
            {error ?? (confirmReplace ? t('trim.replaceWarning') : t('trim.loopHelp'))}
          </span>
          <button type="button" className="btn btn-quiet btn-sm" onClick={close}>
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className={confirmReplace ? 'btn btn-danger-solid btn-sm' : 'btn btn-quiet btn-sm'}
            onClick={() => (confirmReplace ? void save(false) : setConfirmReplace(true))}
            onBlur={() => setConfirmReplace(false)}
            disabled={saving || !total}>
            {confirmReplace ? t('trim.replaceConfirm') : t('trim.replaceOriginal')}
          </button>
          <button
            type="button"
            className="btn btn-sm"
            onClick={() => void save(true)}
            disabled={saving || !total}>
            {saving ? t('trim.saving') : t('trim.saveCopy')}
          </button>
        </div>
      </div>
    </div>
  );
}
