// Re-export shared dungeon types from game_schema.
pub use game_schema::dungeon::*;

use crate::physics_backend::EnvironmentShape;

/// Convert an authoring-side `ShapeDef` into the runtime
/// `EnvironmentShape` consumed by `PhysicsBackend`. Centralises the
/// match arms so call sites (worker startup, instance spawn, future
/// terrain materialiser) stay in sync as new shapes are added.
pub fn shape_def_to_environment(shape: &ShapeDef) -> EnvironmentShape {
    match shape {
        ShapeDef::Cuboid {
            half_x,
            half_y,
            half_z,
        } => EnvironmentShape::Cuboid {
            half_x: *half_x,
            half_y: *half_y,
            half_z: *half_z,
        },
        ShapeDef::Cylinder {
            half_height,
            radius,
        } => EnvironmentShape::Cylinder {
            half_height: *half_height,
            radius: *radius,
        },
        ShapeDef::Heightfield {
            nrows,
            ncols,
            scale_x,
            scale_y,
            scale_z,
            heights,
        } => EnvironmentShape::Heightfield {
            nrows: *nrows,
            ncols: *ncols,
            scale_x: *scale_x,
            scale_y: *scale_y,
            scale_z: *scale_z,
            heights: heights.clone(),
        },
        ShapeDef::TriMesh { vertices, indices } => EnvironmentShape::TriMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
        },
    }
}

/// Registry of parsed dungeon templates, keyed by template_id.
pub struct DungeonRegistry {
    templates: std::collections::HashMap<String, DungeonTemplate>,
}

/// Resolved interactable ready for DB insertion.
///
/// Produced by [`resolve_linked_entities`] — mirrors the second pass of the
/// two-pass spawn in `create_instance` but as a pure, testable function.
pub struct ResolvedInteractable {
    pub local_id: u32,
    pub entity_id: u64,
    pub linked_entity: Option<u64>,
}

/// Given interactable definitions and their assigned entity IDs, resolve
/// `linked_to` local-ID references into real entity IDs.
///
/// `local_to_entity` maps each `InteractableDef.local_id` to the entity ID
/// assigned in the first spawn pass.  A missing or dangling `linked_to`
/// resolves to `None`.
pub fn resolve_linked_entities(
    defs: &[InteractableDef],
    local_to_entity: &std::collections::HashMap<u32, u64>,
) -> Vec<ResolvedInteractable> {
    defs.iter()
        .map(|def| {
            let entity_id = *local_to_entity.get(&def.local_id).unwrap_or_else(|| {
                panic!("Missing entity ID for local_id {}", def.local_id)
            });
            let linked_entity = def
                .linked_to
                .and_then(|lid| local_to_entity.get(&lid).copied());
            ResolvedInteractable {
                local_id: def.local_id,
                entity_id,
                linked_entity,
            }
        })
        .collect()
}

impl DungeonRegistry {
    pub fn new() -> Self {
        Self {
            templates: std::collections::HashMap::new(),
        }
    }

    pub fn register(&mut self, template: DungeonTemplate) {
        self.templates
            .insert(template.template_id.clone(), template);
    }

    pub fn get(&self, template_id: &str) -> Option<&DungeonTemplate> {
        self.templates.get(template_id)
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_RON: &str = r#"(
        templates: [
            (
                template_id: "arena_01",
                name: "Test Arena",
                max_players: 4,
                spawn_points: [(0.0, 1.0, 0.0)],
                geometry: [
                    (shape: Cuboid(half_x: 10.0, half_y: 0.5, half_z: 10.0), position: (0.0, -0.5, 0.0)),
                    (shape: Cylinder(half_height: 3.0, radius: 1.0), position: (5.0, 3.0, 0.0)),
                ],
                interactables: [
                    (local_id: 1, kind: Gate, position: (0.0, 1.5, -5.0), linked_to: None, required_buff: None, required_item: None, interact_range: None),
                    (local_id: 2, kind: Switch, position: (3.0, 1.0, -3.0), linked_to: Some(1), required_buff: None, required_item: None, interact_range: Some(3.0)),
                    (local_id: 3, kind: BossSpawn(npc_name: "Golem", encounter_name: Some("state_enter_demo")), position: (0.0, 1.0, -8.0), linked_to: None, required_buff: None, required_item: None, interact_range: None),
                    (local_id: 4, kind: NpcSpawn(npc_name: "Guard"), position: (2.0, 1.0, 0.0), linked_to: None, required_buff: None, required_item: None, interact_range: None),
                    (local_id: 5, kind: Chest, position: (-2.0, 0.5, -9.0), linked_to: None, required_buff: Some(42), required_item: Some(100), interact_range: Some(2.0)),
                ],
            ),
        ],
    )"#;

    #[test]
    fn parse_dungeon_ron() {
        let file: DungeonFile = ron::from_str(TEST_RON).expect("valid RON");
        assert_eq!(file.templates.len(), 1);

        let t = &file.templates[0];
        assert_eq!(t.template_id, "arena_01");
        assert_eq!(t.name, "Test Arena");
        assert_eq!(t.max_players, 4);
        assert_eq!(t.spawn_points, vec![[0.0, 1.0, 0.0]]);
    }

    #[test]
    fn parse_geometry_shapes() {
        let file: DungeonFile = ron::from_str(TEST_RON).expect("valid RON");
        let geom = &file.templates[0].geometry;
        assert_eq!(geom.len(), 2);

        match &geom[0].shape {
            ShapeDef::Cuboid {
                half_x,
                half_y,
                half_z,
            } => {
                assert_eq!(*half_x, 10.0);
                assert_eq!(*half_y, 0.5);
                assert_eq!(*half_z, 10.0);
            }
            other => panic!("expected Cuboid, got {other:?}"),
        }
        assert_eq!(geom[0].position, [0.0, -0.5, 0.0]);

        match &geom[1].shape {
            ShapeDef::Cylinder {
                half_height,
                radius,
            } => {
                assert_eq!(*half_height, 3.0);
                assert_eq!(*radius, 1.0);
            }
            other => panic!("expected Cylinder, got {other:?}"),
        }
    }

    #[test]
    fn parse_interactable_kinds() {
        let file: DungeonFile = ron::from_str(TEST_RON).expect("valid RON");
        let ints = &file.templates[0].interactables;
        assert_eq!(ints.len(), 5);

        assert!(matches!(ints[0].kind, InteractKindDef::Gate));
        assert!(ints[0].linked_to.is_none());

        assert!(matches!(ints[1].kind, InteractKindDef::Switch));
        assert_eq!(ints[1].linked_to, Some(1)); // linked to gate

        match &ints[2].kind {
            InteractKindDef::BossSpawn {
                npc_name,
                encounter_name,
            } => {
                assert_eq!(npc_name, "Golem");
                assert_eq!(encounter_name.as_deref(), Some("state_enter_demo"));
            }
            other => panic!("expected BossSpawn, got {other:?}"),
        }

        match &ints[3].kind {
            InteractKindDef::NpcSpawn { npc_name } => assert_eq!(npc_name, "Guard"),
            other => panic!("expected NpcSpawn, got {other:?}"),
        }

        assert!(matches!(ints[4].kind, InteractKindDef::Chest));
        assert_eq!(ints[4].required_buff, Some(42));
        assert_eq!(ints[4].required_item, Some(100));
        assert_eq!(ints[4].interact_range, Some(2.0));
    }

    #[test]
    fn registry_insert_and_lookup() {
        let file: DungeonFile = ron::from_str(TEST_RON).expect("valid RON");
        let mut reg = DungeonRegistry::new();
        assert!(reg.is_empty());

        for t in file.templates {
            reg.register(t);
        }
        assert_eq!(reg.len(), 1);

        let t = reg.get("arena_01").expect("should find template");
        assert_eq!(t.name, "Test Arena");
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn parse_shipped_dungeons_ron() {
        let src = include_str!("../../../data/dungeons.ron");
        let file: DungeonFile = ron::from_str(src)
            .expect("data/dungeons.ron embedded at compile time must be valid RON");
        assert!(
            !file.templates.is_empty(),
            "shipped file should have at least one template"
        );

        for t in &file.templates {
            assert!(!t.template_id.is_empty(), "template_id must not be empty");
            assert!(t.max_players > 0, "max_players must be positive");
        }
    }

    #[test]
    fn parse_boss_spawn_without_encounter_name_defaults_none() {
        const LEGACY: &str = r#"(
            templates: [
                (
                    template_id: "legacy",
                    name: "Legacy",
                    max_players: 1,
                    spawn_points: [(0.0, 1.0, 0.0)],
                    geometry: [
                        (shape: Cuboid(half_x: 1.0, half_y: 1.0, half_z: 1.0), position: (0.0, 0.0, 0.0)),
                    ],
                    interactables: [
                        (local_id: 1, kind: BossSpawn(npc_name: "LegacyBoss"), position: (0.0, 1.0, 0.0), linked_to: None, required_buff: None, required_item: None, interact_range: None),
                    ],
                ),
            ],
        )"#;

        let file: DungeonFile = ron::from_str(LEGACY).expect("legacy BossSpawn syntax should parse");
        match &file.templates[0].interactables[0].kind {
            InteractKindDef::BossSpawn {
                npc_name,
                encounter_name,
            } => {
                assert_eq!(npc_name, "LegacyBoss");
                assert!(encounter_name.is_none());
            }
            other => panic!("expected BossSpawn, got {other:?}"),
        }
    }

    // ── Two-pass spawn resolution tests ───────────────────────────────

    fn make_def(local_id: u32, kind: InteractKindDef, linked_to: Option<u32>) -> InteractableDef {
        InteractableDef {
            local_id,
            kind,
            position: [0.0, 0.0, 0.0],
            linked_to,
            required_buff: None,
            required_item: None,
            interact_range: None,
        }
    }

    #[test]
    fn resolve_linked_entities_basic() {
        // Gate(1) ← Switch(2) links to it
        let defs = vec![
            make_def(1, InteractKindDef::Gate, None),
            make_def(2, InteractKindDef::Switch, Some(1)),
        ];
        let mut map = std::collections::HashMap::new();
        map.insert(1, 1000);
        map.insert(2, 1001);

        let resolved = resolve_linked_entities(&defs, &map);
        assert_eq!(resolved.len(), 2);

        // Gate has no link
        assert_eq!(resolved[0].entity_id, 1000);
        assert_eq!(resolved[0].linked_entity, None);

        // Switch links to gate's real entity_id
        assert_eq!(resolved[1].entity_id, 1001);
        assert_eq!(resolved[1].linked_entity, Some(1000));
    }

    #[test]
    fn resolve_linked_entities_dangling_ref() {
        // Switch links to local_id 99 which doesn't exist in the map.
        let defs = vec![make_def(1, InteractKindDef::Switch, Some(99))];
        let mut map = std::collections::HashMap::new();
        map.insert(1, 500);

        let resolved = resolve_linked_entities(&defs, &map);
        assert_eq!(
            resolved[0].linked_entity, None,
            "dangling linked_to should resolve to None"
        );
    }

    #[test]
    fn resolve_linked_entities_chain() {
        // Chest(3) → Switch(2) → Gate(1)
        // Each links to the previous. Resolution is per-def, not transitive.
        let defs = vec![
            make_def(1, InteractKindDef::Gate, None),
            make_def(2, InteractKindDef::Switch, Some(1)),
            make_def(3, InteractKindDef::Chest, Some(2)),
        ];
        let mut map = std::collections::HashMap::new();
        map.insert(1, 10);
        map.insert(2, 20);
        map.insert(3, 30);

        let resolved = resolve_linked_entities(&defs, &map);
        assert_eq!(resolved[0].linked_entity, None);
        assert_eq!(resolved[1].linked_entity, Some(10)); // switch → gate
        assert_eq!(resolved[2].linked_entity, Some(20)); // chest → switch
    }

    #[test]
    fn resolve_linked_entities_from_parsed_ron() {
        // Use the TEST_RON template which has switch(2) → gate(1).
        let file: DungeonFile = ron::from_str(TEST_RON).expect("valid RON");
        let defs = &file.templates[0].interactables;

        // Simulate first pass: assign sequential entity IDs.
        let mut map = std::collections::HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            map.insert(def.local_id, 1000 + i as u64);
        }

        let resolved = resolve_linked_entities(defs, &map);
        assert_eq!(resolved.len(), 5);

        // local_id=1 (Gate) → no link
        assert_eq!(resolved[0].linked_entity, None);
        // local_id=2 (Switch) → linked_to=1 → entity 1000
        assert_eq!(resolved[1].linked_entity, Some(1000));
        // The rest have no links
        assert_eq!(resolved[2].linked_entity, None);
        assert_eq!(resolved[3].linked_entity, None);
        assert_eq!(resolved[4].linked_entity, None);
    }
}
