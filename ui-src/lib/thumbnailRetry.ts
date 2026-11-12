/** Short, bounded retries for transient thumbnail-worker/network failures.
 * Readiness is controlled by viewport, page/network visibility and playback. */
export class ThumbnailRetry {
  private timer: ReturnType<typeof setTimeout> | undefined;
  private ready = false;
  private failed = false;
  private disposed = false;
  private attempts = 0;
  private request = 0;

  private readonly retry: (request: number) => void;
  private readonly random: () => number;
  constructor(retry: (request: number) => void, random = Math.random) {
    this.retry = retry;
    this.random = random;
  }

  setReady(ready: boolean): void {
    if (this.disposed) return;
    const resumed = ready && !this.ready;
    this.ready = ready;
    if (!ready) this.cancel();
    if (resumed) {
      // A new visible visit, reconnect or closed player can recover a thumbnail
      // which exhausted its earlier burst while the source was unavailable.
      this.attempts = 0;
      this.schedule();
    }
  }

  error(): void {
    if (this.disposed) return;
    this.failed = true;
    this.schedule();
  }

  loaded(): void {
    this.failed = false;
    this.attempts = 0;
    this.cancel();
  }

  dispose(): void {
    this.disposed = true;
    this.cancel();
  }

  private cancel(): void {
    clearTimeout(this.timer);
    this.timer = undefined;
  }

  private schedule(): void {
    if (!this.ready || !this.failed || this.disposed || this.timer !== undefined || this.attempts >= 8) return;
    const backoff = [250, 400, 650, 900, 1200, 1600, 1800, 1800];
    const delay = Math.min(2000, backoff[this.attempts] + Math.floor(this.random() * 201));
    this.timer = setTimeout(() => {
      this.timer = undefined;
      if (!this.ready || this.disposed) return;
      this.attempts++;
      this.failed = false;
      this.retry(++this.request);
    }, delay);
  }
}
