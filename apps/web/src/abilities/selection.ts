// Client-side tab/click target selection.
//
// Pure controller — no Three.js / DOM dependencies — so it can be unit-tested.
// Authority lives on the server (see docs/contracts/client_prediction_contract.md
// "Authority" section). Selection is UI state that gets attached to outgoing
// `AbilityTarget.Entity` payloads; the server re-validates every intent.

export type SelectionListener = (state: SelectionState) => void;

export type SelectionState = {
  hover: bigint | undefined;
  selected: bigint | undefined;
};

export type ScreenProjector = (position: { x: number; y: number; z: number }) =>
  | {
      onScreen: boolean;
      ndcX: number;
      ndcY: number;
    }
  | undefined;

export type TabCandidate = {
  entityId: bigint;
  position: { x: number; y: number; z: number };
};

export class TargetSelection {
  private hover: bigint | undefined;
  private selected: bigint | undefined;
  private readonly listeners = new Set<SelectionListener>();

  state(): SelectionState {
    return { hover: this.hover, selected: this.selected };
  }

  getHover(): bigint | undefined {
    return this.hover;
  }

  getSelected(): bigint | undefined {
    return this.selected;
  }

  setHover(id: bigint | undefined): void {
    if (this.hover === id) {
      return;
    }
    this.hover = id;
    this.emit();
  }

  setSelected(id: bigint | undefined): void {
    if (this.selected === id) {
      return;
    }
    this.selected = id;
    this.emit();
  }

  /** Promote hover → selected. Returns true if selection changed. */
  promoteHover(): boolean {
    if (this.hover === undefined) {
      return false;
    }
    if (this.selected === this.hover) {
      return false;
    }
    this.selected = this.hover;
    this.emit();
    return true;
  }

  clear(): void {
    if (this.hover === undefined && this.selected === undefined) {
      return;
    }
    this.hover = undefined;
    this.selected = undefined;
    this.emit();
  }

  /**
   * Reconcile against the current AOI. Selected ids no longer present are dropped.
   */
  reconcile(liveIds: ReadonlySet<bigint>): void {
    let changed = false;
    if (this.hover !== undefined && !liveIds.has(this.hover)) {
      this.hover = undefined;
      changed = true;
    }
    if (this.selected !== undefined && !liveIds.has(this.selected)) {
      this.selected = undefined;
      changed = true;
    }
    if (changed) {
      this.emit();
    }
  }

  /**
   * Cycle to the next candidate. Sort key:
   *   (1) on-screen first
   *   (2) ascending screen-center distance (smaller = more centered)
   *   (3) ascending world distance from `playerPosition`
   *   (4) ascending entityId for stable tie-break
   *
   * Returns the new selected id (or undefined when no candidates).
   */
  tabNext(
    candidates: readonly TabCandidate[],
    playerPosition: { x: number; y: number; z: number },
    project: ScreenProjector,
  ): bigint | undefined {
    if (candidates.length === 0) {
      return undefined;
    }
    const sorted = sortTabCandidates(candidates, playerPosition, project);
    if (sorted.length === 0) {
      return undefined;
    }
    const currentIndex = sorted.findIndex((entry) => entry.entityId === this.selected);
    const next = sorted[(currentIndex + 1) % sorted.length]!;
    if (next.entityId !== this.selected) {
      this.selected = next.entityId;
      this.emit();
    }
    return this.selected;
  }

  subscribe(listener: SelectionListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private emit(): void {
    const snapshot = this.state();
    for (const listener of this.listeners) {
      listener(snapshot);
    }
  }
}

export function sortTabCandidates(
  candidates: readonly TabCandidate[],
  playerPosition: { x: number; y: number; z: number },
  project: ScreenProjector,
): TabCandidate[] {
  type Scored = {
    candidate: TabCandidate;
    onScreen: boolean;
    screenDistSq: number;
    worldDistSq: number;
  };

  const scored: Scored[] = candidates.map((candidate) => {
    const projected = project(candidate.position);
    const onScreen = projected?.onScreen ?? false;
    const screenDistSq = projected
      ? projected.ndcX * projected.ndcX + projected.ndcY * projected.ndcY
      : Number.POSITIVE_INFINITY;
    const dx = candidate.position.x - playerPosition.x;
    const dy = candidate.position.y - playerPosition.y;
    const dz = candidate.position.z - playerPosition.z;
    const worldDistSq = dx * dx + dy * dy + dz * dz;
    return { candidate, onScreen, screenDistSq, worldDistSq };
  });

  scored.sort((a, b) => {
    if (a.onScreen !== b.onScreen) {
      return a.onScreen ? -1 : 1;
    }
    if (a.screenDistSq !== b.screenDistSq) {
      return a.screenDistSq - b.screenDistSq;
    }
    if (a.worldDistSq !== b.worldDistSq) {
      return a.worldDistSq - b.worldDistSq;
    }
    return a.candidate.entityId < b.candidate.entityId ? -1 : 1;
  });

  return scored.map((entry) => entry.candidate);
}
