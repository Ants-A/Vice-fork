interface Transport {
  create: (slug: string) => Promise<{id: string}>;
  touch: (id: string) => Promise<unknown>;
  close: (id: string) => Promise<unknown>;
}

interface Session {
  users: number;
  source: Promise<string>;
  id?: string;
  heartbeat?: ReturnType<typeof setInterval>;
}

/** Shared by the viewer and trim preview. Ending playback never releases a lease. */
export class PlaybackSessions {
  private sessions = new Map<string, Session>();
  private transport: Transport;

  constructor(transport: Transport) {
    this.transport = transport;
  }

  acquire(slug: string, source: string): {source: Promise<string>; release: () => void} {
    let session = this.sessions.get(source);
    if (!session) {
      const created: Session = {users: 0, source: Promise.resolve(source)};
      created.source = this.transport.create(slug).then(({id}) => {
        created.id = id;
        if (!created.users) {
          void this.transport.close(id).catch(() => {});
          return source;
        }
        created.heartbeat = setInterval(() => {
          void this.transport.touch(id).catch(() => {});
        }, 30_000);
        return `${source}${source.includes('?') ? '&' : '?'}playback_session=${encodeURIComponent(id)}`;
      }).catch(() => source);
      session = created;
      this.sessions.set(source, session);
    }
    session.users++;
    const lease = session;
    let released = false;
    return {
      source: lease.source,
      release: () => {
        if (released) return;
        released = true;
        if (--lease.users) return;
        if (this.sessions.get(source) === lease) this.sessions.delete(source);
        if (lease.heartbeat) clearInterval(lease.heartbeat);
        if (lease.id) void this.transport.close(lease.id).catch(() => {});
      },
    };
  }
}
