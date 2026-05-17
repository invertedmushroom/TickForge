use serde::Deserialize;

/// On-disk serialization format for `data/dungeons.ron`.
#[derive(Clone, Debug, Deserialize)]
pub struct DungeonFile {
    pub templates: Vec<DungeonTemplate>,
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
    pub spawn_point: [f32; 3],
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
    Cuboid { half_x: f32, half_y: f32, half_z: f32 },
    Cylinder { half_height: f32, radius: f32 },
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
