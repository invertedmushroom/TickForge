import type { DbConnection, SubscriptionHandle } from '@dive/client-contract/bindings';

export type FeatureSubscriptionStats = {
  alwaysOnCount: number;
  availableFeatureCount: number;
  activeFeatureCount: number;
  activeFeatureSubscriptions: number;
  totalActiveCount: number;
  activeFeatures: string[];
};

type FeatureEntry = {
  queries: string[];
  refs: number;
  handle?: SubscriptionHandle;
};

type SubscriptionErrorHandler = (message: string) => void;
type SubscriptionAppliedHandler = () => void;

export class FeatureSubscriptionManager {
  private connection?: DbConnection;
  private readonly allowedQueries: Set<string>;
  private readonly entries = new Map<string, FeatureEntry>();

  constructor(
    allowedFeatureSubscriptions: readonly string[],
    private readonly onError: SubscriptionErrorHandler = () => undefined,
    private readonly onApplied: SubscriptionAppliedHandler = () => undefined,
  ) {
    this.allowedQueries = new Set(allowedFeatureSubscriptions.map(normalizeSubscriptionQuery));
  }

  setConnection(connection: DbConnection | undefined): void {
    if (this.connection === connection) {
      return;
    }

    this.unsubscribeAll();
    this.connection = connection;

    if (!connection) {
      return;
    }

    for (const entry of this.entries.values()) {
      this.subscribeEntry(entry);
    }
  }

  retain(featureName: string, queries: readonly string[]): () => void {
    const feature = featureName.trim();
    if (!feature) {
      throw new Error('feature subscription name is required');
    }

    assertKnownFeatureSubscriptions(queries, this.allowedQueries);
    const normalizedQueries = queries.map(normalizeSubscriptionQuery).sort();
    const existing = this.entries.get(feature);

    if (existing) {
      const existingQueries = existing.queries.map(normalizeSubscriptionQuery).sort();
      if (existingQueries.join('\n') !== normalizedQueries.join('\n')) {
        throw new Error(`feature subscription "${feature}" was already retained with different queries`);
      }
      existing.refs += 1;
      return this.releaseOnce(feature);
    }

    const entry: FeatureEntry = {
      queries: [...queries],
      refs: 1,
    };
    this.entries.set(feature, entry);
    this.subscribeEntry(entry);
    return this.releaseOnce(feature);
  }

  stats(alwaysOnCount: number): FeatureSubscriptionStats {
    let activeFeatureSubscriptions = 0;
    for (const entry of this.entries.values()) {
      activeFeatureSubscriptions += entry.queries.length;
    }

    return {
      alwaysOnCount,
      availableFeatureCount: this.allowedQueries.size,
      activeFeatureCount: this.entries.size,
      activeFeatureSubscriptions,
      totalActiveCount: alwaysOnCount + activeFeatureSubscriptions,
      activeFeatures: Array.from(this.entries.keys()).sort(),
    };
  }

  destroy(): void {
    this.unsubscribeAll();
    this.entries.clear();
    this.connection = undefined;
  }

  private releaseOnce(feature: string): () => void {
    let released = false;
    return () => {
      if (released) {
        return;
      }
      released = true;
      const entry = this.entries.get(feature);
      if (!entry) {
        return;
      }
      entry.refs -= 1;
      if (entry.refs > 0) {
        return;
      }
      unsubscribeHandle(entry);
      this.entries.delete(feature);
    };
  }

  private subscribeEntry(entry: FeatureEntry): void {
    if (!this.connection || entry.handle) {
      return;
    }

    entry.handle = this.connection
      .subscriptionBuilder()
      .onApplied(() => this.onApplied())
      .onError((ctx) => this.onError(subscriptionErrorMessage(ctx)))
      .subscribe(entry.queries);
  }

  private unsubscribeAll(): void {
    for (const entry of this.entries.values()) {
      unsubscribeHandle(entry);
    }
  }
}

export function assertKnownFeatureSubscriptions(queries: readonly string[], allowedQueries: ReadonlySet<string>): void {
  for (const query of queries) {
    if (!allowedQueries.has(normalizeSubscriptionQuery(query))) {
      throw new Error(`unknown browser feature subscription: ${query}`);
    }
  }
}

export function normalizeSubscriptionQuery(query: string): string {
  return query.trim().replace(/\s+/g, ' ').toLowerCase();
}

export function subscriptionTableName(query: string): string | undefined {
  return /\bfrom\s+([a-zA-Z_][a-zA-Z0-9_]*)\b/i.exec(query)?.[1];
}

export function subscriptionErrorMessage(ctx: unknown): string {
  if (ctx && typeof ctx === 'object' && 'event' in ctx) {
    return `subscription error: ${String(ctx.event)}`;
  }
  return 'subscription error';
}

function unsubscribeHandle(entry: FeatureEntry): void {
  entry.handle?.unsubscribe();
  entry.handle = undefined;
}
