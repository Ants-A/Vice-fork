/** Hide a confirmed deletion while its request runs; restore it on failure. */
export class PendingDeletes {
  private requests = new Map<string, Promise<void>>();

  private changed: (keys: ReadonlySet<string>) => void;

  constructor(changed: (keys: ReadonlySet<string>) => void) {
    this.changed = changed;
  }

  run(key: string, remove: () => Promise<void>, committed: () => void): Promise<void> {
    const existing = this.requests.get(key);
    if (existing) return existing;
    const request = Promise.resolve().then(remove).then(committed).finally(() => {
      this.requests.delete(key);
      this.changed(new Set(this.requests.keys()));
    });
    this.requests.set(key, request);
    this.changed(new Set(this.requests.keys()));
    return request;
  }
}
