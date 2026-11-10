/** Hover previews share one player budget, and release network reads on leave. */
let active: HTMLVideoElement | null = null;
let pending: {video: HTMLVideoElement; timer: ReturnType<typeof setTimeout>} | null = null;

export function stopHoverPreview(video?: HTMLVideoElement): void {
  if (pending && (!video || pending.video === video)) {
    clearTimeout(pending.timer);
    pending = null;
  }
  if (active && (!video || active === video)) {
    const previous = active;
    active = null;
    previous.pause();
    previous.removeAttribute('src');
    previous.load();
  }
}

export function startHoverPreview(video: HTMLVideoElement, source: string, onFailure: () => void): void {
  stopHoverPreview();
  pending = {video, timer: setTimeout(() => {
    pending = null;
    if (!video.isConnected) return;
    active = video;
    video.src = source;
    void video.play().catch(() => {
      if (active === video && video.getAttribute('src')) onFailure();
    });
  }, 400)};
}
