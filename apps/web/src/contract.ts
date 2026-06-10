import { DbConnection } from '@dive/client-contract/bindings';
import abilityMetadata from '@dive/client-contract/abilities.json' with { type: 'json' };
import browserPolicy from '@dive/client-contract/browser-policy.json';
import contractManifest from '@dive/client-contract/contract.json';
import contentLookup from '@dive/client-contract/content-lookup.json';
import contentMetadata from '@dive/client-contract/content-metadata.json';
import physicsPrediction from '@dive/client-contract/physics-prediction.json' with { type: 'json' };

export const MODULE_NAME = 'tickforge';
export const DEFAULT_STDB_URI =
  typeof window !== 'undefined'
    ? `ws://${window.location.hostname}:3000`
    : 'ws://127.0.0.1:3000';
export const CONTRACT_MANIFEST = contractManifest;
export const BROWSER_POLICY = browserPolicy;
export const ALWAYS_ON_SUBSCRIPTIONS = browserPolicy.always_on_subscriptions;
export const FEATURE_SUBSCRIPTIONS = browserPolicy.feature_subscriptions;
export const FORBIDDEN_TABLES = browserPolicy.forbidden_tables;
export const ALLOWED_REDUCERS = browserPolicy.allowed_reducers;
export const FORBIDDEN_REDUCERS = browserPolicy.forbidden_reducers;

export type ContractStatus = {
  packageName: string;
  version: string;
  schemaHash: string;
  contentHash: string;
  metadataHash: string;
  physicsHash: string;
  layers: number;
  dungeonTemplates: number;
  terrainSets: number;
  alwaysOnSubscriptions: number;
  featureSubscriptions: number;
  forbiddenTables: number;
  allowedReducers: number;
  bindingImportReady: boolean;
  abilityCount: number;
  abilityModes: string[];
  abilityMetadataPresent: boolean;
};

export class ClientContractMismatchError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ClientContractMismatchError';
  }
}

export function validateStartupContract(): ContractStatus {
  const expectedSchemaHash = import.meta.env.VITE_EXPECTED_SCHEMA_HASH as string | undefined;
  const expectedContentHash = import.meta.env.VITE_EXPECTED_CONTENT_HASH as string | undefined;
  const expectedPhysicsHash = import.meta.env.VITE_EXPECTED_PHYSICS_HASH as string | undefined;

  if (expectedSchemaHash && expectedSchemaHash !== contractManifest.schema_hash) {
    throw new ClientContractMismatchError(
      `client/server schema mismatch: expected ${expectedSchemaHash}, got ${contractManifest.schema_hash}`,
    );
  }

  if (expectedContentHash && expectedContentHash !== contractManifest.content_hash) {
    throw new ClientContractMismatchError(
      `client/server content mismatch: expected ${expectedContentHash}, got ${contractManifest.content_hash}`,
    );
  }

  if (expectedPhysicsHash && expectedPhysicsHash !== contractManifest.physics_hash) {
    throw new ClientContractMismatchError(
      `client/server physics mismatch: expected ${expectedPhysicsHash}, got ${contractManifest.physics_hash}`,
    );
  }

  if (abilityMetadata.schema_version !== 1 || abilityMetadata.abilities.length === 0) {
    throw new ClientContractMismatchError('client contract ability metadata is missing or unsupported');
  }

  if (physicsPrediction.schema_version !== 1 || physicsPrediction.hash !== contractManifest.physics_hash) {
    throw new ClientContractMismatchError('client contract physics prediction metadata is missing or stale');
  }

  return {
    packageName: contractManifest.package_name,
    version: contractManifest.contract_version,
    schemaHash: contractManifest.schema_hash,
    contentHash: contractManifest.content_hash,
    metadataHash: contractManifest.metadata_hash,
    physicsHash: contractManifest.physics_hash,
    layers: contentMetadata.layers.length,
    dungeonTemplates: contentMetadata.dungeon_templates.length,
    terrainSets: contentLookup.terrain_sets.length,
    alwaysOnSubscriptions: ALWAYS_ON_SUBSCRIPTIONS.length,
    featureSubscriptions: FEATURE_SUBSCRIPTIONS.length,
    forbiddenTables: FORBIDDEN_TABLES.length,
    allowedReducers: ALLOWED_REDUCERS.length,
    bindingImportReady: typeof DbConnection.builder === 'function',
    abilityCount: abilityMetadata.abilities.length,
    abilityModes: Array.from(new Set(abilityMetadata.abilities.map((ability) => ability.targeting_mode.kind))).sort(),
    abilityMetadataPresent: true,
  };
}

export function shortHash(hash: string): string {
  return hash.slice(0, 12);
}
