import {onWsMessage} from './ws';
import {api} from './api';
import {copyToClipboard} from './clipboard';
import type {Clip} from './types';
import type {IslandEvent} from '../state/store';
import {t} from './i18n';

/**
 * Copy a clip's share link, saying plainly when the link only works on this
 * network. A LAN address looks identical to a real share link right up until
 * a friend cannot open it (#105).
 *
 * `onManualCopy` is the fallback for windows with no working clipboard: the
 * link has to be put somewhere the user can select it by hand.
 */
export async function copyShareLink(
  clip: Clip,
  notify: (event: Omit<IslandEvent, 'id'>) => void,
  onManualCopy: (url: string) => void,
): Promise<void> {
  if (!clip.share_url) {
    notify({
      kind: 'error',
      title: t('card.noShareLinkYet'),
      tone: 'error',
      holdMs: 4000,
    });
    return;
  }
  if (!(await copyToClipboard(clip.share_url))) {
    onManualCopy(clip.share_url);
    return;
  }
  if (clip.share_is_public === false) {
    notify({
      kind: 'info',
      title: t('card.linkCopiedLocal'),
      detail: t('card.linkCopiedLocalDetail'),
      tone: 'neutral',
      holdMs: 8000,
    });
  } else {
    notify({kind: 'info', title: t('card.shareLinkCopied'), tone: 'accent', holdMs: 3000});
  }
}

/**
 * What the Share button does. By default it copies the link; with
 * `sharing.share_discord_file` it copies a Discord-sized copy of the clip,
 * which can take a while the first time because it has to be encoded.
 */
export async function shareClip(
  clip: Clip,
  discordFile: boolean,
  notify: (event: Omit<IslandEvent, 'id'>) => void,
  onManualCopy: (url: string) => void,
): Promise<void> {
  if (!discordFile) {
    await copyShareLink(clip, notify, onManualCopy);
    return;
  }
  notify({kind: 'info', title: t('card.discordPreparing'), tone: 'neutral', holdMs: 600_000});
  const unsubscribe = onWsMessage(msg => {
    if (msg.type === 'discord_progress' && msg.slug === clip.slug) {
      notify({kind: 'info', title: t('card.discordPreparing'), detail: `${Math.round(msg.progress * 100)}%`, tone: 'neutral', holdMs: 600_000});
    }
  });
  try {
    await api.copyClipFile(clip.slug, {discord: true});
    notify({
      kind: 'info',
      title: t('card.discordCopied'),
      detail: t('card.discordCopiedDetail'),
      tone: 'accent',
      holdMs: 4000,
    });
  } catch (err) {
    notify({
      kind: 'error',
      title: t('card.errDiscordCopy'),
      detail: (err as Error).message,
      tone: 'error',
      holdMs: 7000,
    });
  } finally {
    unsubscribe();
  }
}

/** The Share button's tooltip, which also has to say that right-click exists. */
export function shareButtonLabel(discordFile: boolean): string {
  return t('card.shareTitle', {action: discordFile ? t('card.copyForDiscord') : t('card.copyShareLink')});
}
