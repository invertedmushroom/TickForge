use spacetimedb::{Filter, client_visibility_filter};

/// Only the trusted simulation worker can subscribe to raw entity tables.
/// Game clients access these through per-user AOI views (nearby_transforms, etc.).
///
/// SpacetimeDB's subscription engine requires indexed join columns (cross-joins
/// are unsupported). We use a sentinel `rls_group` column (always 0, btree-indexed)
/// on both sides to provide a valid equi-join. The WHERE clause then restricts
/// results to identities present in `trusted_worker`.

#[client_visibility_filter]
const ENTITY_TRANSFORM_ACCESS: Filter = Filter::Sql(
    "SELECT entity_transform.* FROM entity_transform JOIN trusted_worker ON entity_transform.rls_group = trusted_worker.rls_group WHERE trusted_worker.worker_identity = :sender",
);

#[client_visibility_filter]
const ENTITY_HEALTH_ACCESS: Filter = Filter::Sql(
    "SELECT entity_health.* FROM entity_health JOIN trusted_worker ON entity_health.rls_group = trusted_worker.rls_group WHERE trusted_worker.worker_identity = :sender",
);

#[client_visibility_filter]
const ENTITY_ACCESS: Filter = Filter::Sql(
    "SELECT entity.* FROM entity JOIN trusted_worker ON entity.rls_group = trusted_worker.rls_group WHERE trusted_worker.worker_identity = :sender",
);
