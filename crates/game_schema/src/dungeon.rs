use serde::{Deserialize, Serialize};

/// Per-layer collision policy controlling which entity interactions are
/// permitted. Stored on dungeon templates, copied into `Instance` rows,
/// and cached by the physics runtime for scene-query predicates.
///
/// Defaults: no player-vs-player physical collision.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub struct LayerCollisionPolicy {
    /// Whether player character bodies physically collide with each other
    /// (KCC movement, raycasts, etc.). `false` for open-world PvE,
    /// `true` for PvP arenas and dungeons.
    pub player_collides_player: bool,
}

impl Default for LayerCollisionPolicy {
    fn default() -> Self {
        Self {
            player_collides_player: false,
        }
    }
}

/// On-disk serialization format for `data/dungeons.ron`.
#[derive(Clone, Debug, Deserialize)]
pub struct DungeonFile {
    pub templates: Vec<DungeonTemplate>,
}

/// On-disk serialization format for `data/layers.ron` — defines the
/// **named static layers** that exist independently of dungeon
/// instances (open-world, social hubs, persistent test rooms, etc).
///
/// Static layers must use IDs in the reserved range `0..100`; dynamic
/// dungeon instance layers are allocated from `100+` by
/// `create_instance` (see `ModuleConfig.next_instance_layer`).
#[derive(Clone, Debug, Deserialize)]
pub struct WorldLayersFile {
    pub layers: Vec<WorldLayerDef>,
}

/// A pre-allocated static layer (e.g. open world, hub) authored in
/// `data/layers.ron`. Materialised at worker startup using the same
/// `GeometryDef + Option<terrain_set>` shape as `DungeonTemplate`,
/// so RON-authored fixtures (walls, pillars, heightfield tiles) and
/// editor-baked voxel terrain compose on the same layer.
///
/// Unlike a `DungeonTemplate`, a `WorldLayerDef` has a fixed `layer_id`
/// and no `Instance` row — entities reach it via direct teleport
/// (`leave_instance`, world-portal triggers, …) rather than
/// `create_instance` / `join_instance`.
#[derive(Clone, Debug, Deserialize)]
pub struct WorldLayerDef {
    /// Stable layer id in `0..100`. The worker rejects values `>= 100`
    /// to keep the dynamic-instance allocator disjoint.
    pub layer_id: u32,
    pub name: String,
    /// Hand-authored geometry (walls, pillars, hand-tuned heightfield
    /// tiles). Always materialised; combines additively with any
    /// terrain set referenced by `terrain_set`.
    #[serde(default)]
    pub geometry: Vec<GeometryDef>,
    /// Optional reference to a baked voxel terrain set. When Some,
    /// the worker also loads the matching `terrain_chunk` rows and
    /// adds them as TriMesh colliders on this layer. Authoring of
    /// terrain rows is offline / editor-side — see
    /// `docs/plan/plan.md` §4.8b.
    #[serde(default)]
    pub terrain_set: Option<String>,
    /// Per-layer collision rules. Defaults to
    /// `LayerCollisionPolicy::default()` (no PvP).
    #[serde(default)]
    pub collision_policy: LayerCollisionPolicy,
    /// Optional spawn points for `respawn_player` / portal arrivals.
    /// Empty vec means the layer is not directly spawnable into.
    #[serde(default)]
    pub spawn_points: Vec<[f32; 3]>,
}

/// A reusable dungeon blueprint.  Each template describes the static geometry
/// (walls, floors, pillars) and interactable objects (gates, switches, bosses,
/// NPC spawns) that make up an instance.
///
/// Geometry is spawned as parentless environment colliders (no entity row) and
/// tagged with the instance's assigned layer so they can be bulk-removed when
/// the instance expires.
///
/// Interactables are spawned as Prop entities with an `interactable_config`
/// companion row.  Gates are kinematic bodies whose collider is toggled via
/// `set_collider_enabled()`.
#[derive(Clone, Debug, Deserialize)]
pub struct DungeonTemplate {
    pub template_id: String,
    pub name: String,
    pub max_players: u32,
    pub geometry: Vec<GeometryDef>,
    pub interactables: Vec<InteractableDef>,
    /// Spawn locations used when an entity enters an instance of this
    /// template (`join_instance`, `debug_join_instance`). The first
    /// entry is used by default; future code may scatter party members
    /// across the list. Empty vec is treated as "no authored spawn"
    /// and the resolver falls back to `[0.0, 1.0, 0.0]`.
    #[serde(default)]
    pub spawn_points: Vec<[f32; 3]>,
    /// Optional designer-authored exit destinations on layer 0 used by
    /// `leave_instance`. First entry wins; empty vec falls back to the
    /// open-world layer's `spawn_points` via the unified resolver.
    #[serde(default)]
    pub exit_points: Vec<[f32; 3]>,
    /// Per-layer collision rules for instances of this template.
    /// Defaults to `LayerCollisionPolicy::default()` when omitted in RON.
    #[serde(default)]
    pub collision_policy: LayerCollisionPolicy,
    /// Optional reference to a baked voxel terrain set. When `Some`,
    /// the worker materialises the matching `terrain_chunk` rows on the
    /// instance's layer in addition to `geometry` above. Many instances
    /// can share one terrain set; rows live once and are referenced by
    /// id. See `docs/plan/plan.md` §4.8b.
    #[serde(default)]
    pub terrain_set: Option<String>,
}

/// A piece of static environment geometry (wall, floor, pillar).
#[derive(Clone, Debug, Deserialize)]
pub struct GeometryDef {
    pub shape: ShapeDef,
    pub position: [f32; 3],
}

/// Abstract shapes that map to `EnvironmentShape` at runtime.
#[derive(Clone, Debug, Deserialize)]
pub enum ShapeDef {
    Cuboid {
        half_x: f32,
        half_y: f32,
        half_z: f32,
    },
    Cylinder {
        half_height: f32,
        radius: f32,
    },
    /// Heightfield terrain on the x-z plane.
    ///
    /// `nrows` × `ncols` grid of heights (rows span x, cols span z). The grid
    /// is stretched to fill a box of size `(scale_x, scale_y, scale_z)` centred
    /// on the collider's `position`. `scale_y` multiplies the raw `heights`
    /// values — use `1.0` if heights are already in world units.
    ///
    /// `heights` MUST have length `nrows * ncols` in parry's column-major
    /// order: `heights[i + j * nrows]` == sample at row `i`, col `j`.
    Heightfield {
        nrows: usize,
        ncols: usize,
        scale_x: f32,
        scale_y: f32,
        scale_z: f32,
        heights: Vec<f32>,
    },
    /// Indexed triangle mesh. `vertices` is a flat `[x, y, z, x, y, z, ...]`
    /// vector (length must be a multiple of 3). `indices` is a flat
    /// `[i0, i1, i2, i0, i1, i2, ...]` triangle list (length must be a
    /// multiple of 3, and every index must be `< vertices.len() / 3`).
    ///
    /// Used for editor-baked open-world / cave geometry. Hand-authored
    /// dungeon RON files normally use `Cuboid`, `Cylinder`, or
    /// `Heightfield`; `TriMesh` is here so the same `ShapeDef` enum
    /// can describe terrain colliders sourced from the voxel pipeline
    /// (see `docs/plan/plan.md` §4.8b).
    TriMesh {
        vertices: Vec<f32>,
        indices: Vec<u32>,
    },
}

/// An interactable object placed inside a dungeon instance.
///
/// `local_id` is a template-scoped identifier so that switches can reference
/// gates via `linked_to` before the real entity IDs are assigned at runtime.
#[derive(Clone, Debug, Deserialize)]
pub struct InteractableDef {
    pub local_id: u32,
    pub kind: InteractKindDef,
    pub position: [f32; 3],
    pub linked_to: Option<u32>,
    pub required_buff: Option<u32>,
    pub required_item: Option<u32>,
    pub interact_range: Option<f32>,
}

#[derive(Clone, Debug, Deserialize)]
pub enum InteractKindDef {
    Gate,
    Switch,
    BossSpawn { npc_name: String },
    NpcSpawn { npc_name: String },
    Chest,
}
