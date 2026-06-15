import type { StdbSnapshot } from '../stdb/connection';

export type DiagnosticsPanel = {
  render: (snapshot: StdbSnapshot, sceneStats?: Window['__DIVE_WEB_SCENE_STATS__']) => void;
  setMapStatus: (text: string, isError?: boolean) => void;
};

export function createDiagnosticsPanel(options: {
  connectionStatus: HTMLElement;
  inputStatus: HTMLElement;
  predictionStatus: HTMLElement;
  remoteStatus: HTMLElement;
  mapStatus: HTMLElement;
  gameplayHud: { render: (snapshot: StdbSnapshot) => void };
}): DiagnosticsPanel {
  const { connectionStatus, inputStatus, predictionStatus, remoteStatus, mapStatus, gameplayHud } = options;

  return {
    render(snapshot, sceneStats) {
      const identity = snapshot.identityShort ? ` · id ${snapshot.identityShort}` : '';
      const entity = snapshot.entityId !== undefined ? ` · entity ${snapshot.entityId}` : '';
      const tick = snapshot.latestTick !== undefined ? ` · tick ${snapshot.latestTick}` : '';
      const layer = snapshot.layer !== undefined ? ` · layer ${snapshot.layer}` : '';
      const instance = snapshot.ownInstance ? ` · inst ${snapshot.ownInstance.instanceId}` : '';
      const error = snapshot.error ? ` · ${snapshot.error}` : '';
      const stall = snapshot.serverStalled ? ` · stalled ${formatMilliseconds(snapshot.tickStallMs)}` : '';
      const reconnects = snapshot.reconnectAttempts > 0 ? ` · reconnect ${snapshot.reconnectAttempts}` : '';

      connectionStatus.textContent = `stdb: ${snapshot.state}${identity}${entity}${tick}${layer}${instance}${stall}${reconnects}${error}`;
      connectionStatus.classList.toggle('diagnostics__line--error', snapshot.state === 'error' || snapshot.serverStalled);

      inputStatus.textContent =
        `input: ack ${snapshot.inputQueue.highestAckedSequence}` +
        ` · next ${snapshot.inputQueue.nextSequenceId}` +
        ` · pending ${snapshot.inputQueue.pendingCount}` +
        ` · resend ${snapshot.inputQueue.resendCount}` +
        ` · rejects ${formatRejects(snapshot.inputQueue.rejectCounts)}`;

      predictionStatus.textContent =
        `prediction: err ${formatMeters(sceneStats?.reconcileErrEwma)}/${formatMeters(sceneStats?.reconcileErrMax)}` +
        ` · snaps ${sceneStats?.snapCorrections ?? 0}` +
        ` · replay ${sceneStats?.replayedInputs ?? 0}` +
        `/${formatRatio(sceneStats?.replayBlockedRatio)}` +
        ` · lead ${formatMeters(sceneStats?.targetLeadMeters)}` +
        ` · auth ${formatBigInt(sceneStats?.lastAuthoritativeTick)}` +
        ` · kcc ${sceneStats?.kccGrounded ? 'ground' : 'air'}` +
        `/${sceneStats?.kccCollisions ?? 0}` +
        ` · hblock ${formatRatio(sceneStats?.kccHorizontalBlockedRatio ?? sceneStats?.kccBlockedRatio)}` +
        ` · floor ${formatRatio(sceneStats?.kccVerticalBlockedRatio)}` +
        ` · kcc ${formatOptionalMilliseconds(sceneStats?.kccMoveMs)}` +
        ` · replay ${formatOptionalMilliseconds(sceneStats?.replayPhysicsMs)}` +
        ` · ray ${formatOptionalMilliseconds(sceneStats?.aimRaycastMs)}` +
        ` · vy ${formatMetersPerSecond(sceneStats?.kccVerticalVelocity)}`;

      remoteStatus.textContent =
        `remote: entities ${sceneStats?.remoteEntities ?? snapshot.remoteEntities.size}` +
        ` · hp ${snapshot.healthRows}` +
        ` · death ${snapshot.deathRows}` +
        ` · events ${snapshot.combatEventCount}/${snapshot.worldEventCount}` +
        ` · pub ${formatHertz(snapshot.metrics.publishRateHz)}` +
        ` · snap ${formatOptionalMilliseconds(snapshot.metrics.snapshotBuildEwmaMs)}` +
        `/${formatOptionalMilliseconds(snapshot.metrics.snapshotBuildMaxMs)}` +
        ` · evt ${formatOptionalMilliseconds(snapshot.metrics.eventMergeMs)}` +
        `/${formatOptionalMilliseconds(snapshot.metrics.eventOrderMs)}` +
        `/${formatOptionalMilliseconds(snapshot.metrics.eventSummaryMs)}` +
        ` · ingest ${formatOptionalMilliseconds(sceneStats?.remoteIngestMs)}` +
        ` · extrap ${sceneStats?.extrapolationEvents ?? 0}` +
        `/${formatSeconds(sceneStats?.extrapolationSecsCurrent)}` +
        `/${formatSeconds(sceneStats?.extrapolationSecsMax)}` +
        ` · gap ${formatSeconds(sceneStats?.snapshotGapEwma)}/${formatSeconds(sceneStats?.snapshotGapMax)}`;

      gameplayHud.render(snapshot);
    },

    setMapStatus(text, isError = false) {
      mapStatus.textContent = text;
      mapStatus.classList.toggle('diagnostics__line--error', isError);
    },
  };
}

function formatRejects(rejectCounts: StdbSnapshot['inputQueue']['rejectCounts']): string {
  const active = Object.entries(rejectCounts)
    .filter(([, count]) => count > 0)
    .map(([kind, count]) => `${kind}:${count}`);
  return active.length > 0 ? active.join(',') : 'none';
}

function formatMeters(value: number | undefined): string {
  if (value === undefined) {
    return '0.00m';
  }
  return `${value.toFixed(2)}m`;
}

function formatSeconds(value: number | undefined): string {
  if (value === undefined) {
    return '0.00s';
  }
  return `${value.toFixed(2)}s`;
}

function formatBigInt(value: bigint | undefined): string {
  return value === undefined ? '-' : `${value}`;
}

function formatRatio(value: number | undefined): string {
  if (value === undefined) {
    return '0%';
  }
  return `${Math.round(value * 100)}%`;
}

function formatHertz(value: number | undefined): string {
  if (value === undefined) {
    return '0hz';
  }
  return `${Math.round(value * 10) / 10}hz`;
}

function formatMetersPerSecond(value: number | undefined): string {
  if (value === undefined) {
    return '0.00m/s';
  }
  return `${value.toFixed(2)}m/s`;
}

function formatMilliseconds(value: number): string {
  return `${Math.round(value)}ms`;
}

function formatOptionalMilliseconds(value: number | undefined): string {
  if (value === undefined) {
    return '0ms';
  }
  return `${Math.round(value * 10) / 10}ms`;
}
