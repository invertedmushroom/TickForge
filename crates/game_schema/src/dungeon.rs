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
    /// Optional client-side visual mesh override. The Bevy client loads
    /// `assets/terrain/{client_visual}/{client_visual}.gltf` when set,
    /// otherwise falls back to the `terrain_set` name. Use this to point
    /// at a higher-poly artist-authored mesh while the server keeps a
    /// coarser baked collision set, or to share one visual across
    /// multiple collision sets. Server-only field — the worker never
    /// reads it.
    #[serde(default)]
    pub client_visual: Option<String>,
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

/// Authoring-friendly catalog of physics body shapes.
///
/// Mirrors `game_core::physics_backend::BodyShape` 1:1 — `to_u8()`
/// returns the same discriminants used by `NpcConfig.body_shape` /
/// `InteractableConfig.body_shape`. Kept in `game_schema` so dungeon
/// RON files can name shapes without depending on `game_core`.
///
/// Stable ordering: values must not be reordered.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum BodyShapeDef {
    PlayerCapsule,
    NpcCapsule,
    BossCapsule,
    LargeBossCapsule,
    GateCuboid,
    SwitchCuboid,
    ChestCuboid,
    CrateCuboid,
}

impl BodyShapeDef {
    /// Encode as the same `u8` discriminant used by
    /// `game_core::physics_backend::BodyShape::to_u8`.
    pub fn to_u8(self) -> u8 {
        match self {
            BodyShapeDef::PlayerCapsule => 0,
            BodyShapeDef::NpcCapsule => 1,
            BodyShapeDef::BossCapsule => 2,
            BodyShapeDef::LargeBossCapsule => 3,
            BodyShapeDef::GateCuboid => 4,
            BodyShapeDef::SwitchCuboid => 5,
            BodyShapeDef::ChestCuboid => 6,
            BodyShapeDef::CrateCuboid => 7,
        }
    }
}

/// An interactable object placed inside a dungeon instance.
///
/// `local_id` is a template-scoped identifier so that switches can reference
/// gates via `linked_to` before the real entity IDs are assigned at runtime.
#[derive(Clone, Debug, Deserialize)]
pub struct InteractableDef {
    pub local_id: u32,
    #[serde(default)]
    pub script_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub kind: InteractKindDef,
    pub position: [f32; 3],
    pub linked_to: Option<u32>,
    pub required_buff: Option<u32>,
    pub required_item: Option<u32>,
    pub interact_range: Option<f32>,
    #[serde(default)]
    pub puzzle_group: Option<String>,
    #[serde(default)]
    pub puzzle_required_count: Option<u32>,
    #[serde(default)]
    pub puzzle_window_ticks: Option<u32>,
    /// Optional explicit body shape. When `None`, the dungeon loader
    /// picks a default from `kind` (Gate → GateCuboid, Switch →
    /// SwitchCuboid, Chest → ChestCuboid, BossSpawn → BossCapsule,
    /// NpcSpawn → NpcCapsule).
    #[serde(default)]
    pub body_shape: Option<BodyShapeDef>,
}

#[derive(Clone, Debug, Deserialize)]
pub enum InteractKindDef {
    Gate,
    Switch,
    BossSpawn {
        npc_name: String,
        encounter_name: Option<String>,
    },
    NpcSpawn {
        npc_name: String,
    },
    Chest,
}
