/** Reconcile an HTTP list with live changes that arrived while it was loading. */
export class ListSnapshots<T extends {slug: string}> {
  private revision = 0;
  private request = 0;
  private applied = 0;
  private active = new Set<number>();
  private changes = new Map<string, {revision: number; value: T | null}>();

  begin(): {revision: number; request: number} {
    const mark = {revision: this.revision, request: ++this.request};
    this.active.add(mark.request);
    return mark;
  }

  observe(slug: string, value: T | null): void {
    this.revision++;
    if (this.active.size) this.changes.set(slug, {revision: this.revision, value});
  }

  finish(mark: {revision: number; request: number}, snapshot: T[] | null): T[] | null {
    this.active.delete(mark.request);
    try {
      if (!snapshot || mark.request < this.applied) return null;
      this.applied = mark.request;
      const changes = new Map([...this.changes].filter(([, change]) => change.revision > mark.revision));
      const seen = new Set(snapshot.map(row => row.slug));
      const added = [...changes.values()].reverse().flatMap(({value}) => value && !seen.has(value.slug) ? [value] : []);
      return [...added, ...snapshot.flatMap(row => {
        const change = changes.get(row.slug);
        return change ? change.value ? [{...row, ...change.value}] : [] : [row];
      })];
    } finally {
      if (!this.active.size) this.changes.clear();
    }
  }
}
