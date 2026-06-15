declare module '@dive/client-contract/contract.json' {
  const value: {
    package_name: string;
    contract_version: string;
    schema_hash: string;
    content_hash: string;
    metadata_hash: string;
    physics_hash: string;
    bindings_dir: string;
    browser_policy_file: string;
    abilities_file: string;
    physics_prediction_file: string;
    content_metadata_file: string;
    content_lookup_file: string;
    map_bundles_dir: string;
    layer_count: number;
    dungeon_template_count: number;
    source_files: string[];
  };
  export default value;
}

declare module '@dive/client-contract/physics-prediction.json' {
  const value: {
    schema_version: number;
    tick_rate_hz: number;
    fixed_dt_seconds: number;
    player_capsule: {
      half_height: number;
      radius: number;
    };
    kcc: {
      offset_relative: number;
      normal_nudge_factor: number;
      max_slope_climb_radians: number;
      snap_to_ground_relative: number;
      autostep_max_height_relative: number;
      autostep_min_width_relative: number;
      autostep_include_dynamic_bodies: boolean;
      ground_pull_meters_per_second: number;
      gravity_meters_per_second_squared: number;
      move_shape_dt_seconds: number;
    };
    collision_groups: {
      movement_membership_bits: number;
      movement_filter_bits: number;
    };
    hash: string;
  };
  export default value;
}

declare module '@dive/client-contract/browser-policy.json' {
  const value: {
    schema_version: number;
    always_on_subscriptions: string[];
    feature_subscriptions: string[];
    forbidden_tables: string[];
    allowed_reducers: string[];
    forbidden_reducers: string[];
  };
  export default value;
}

declare module '@dive/client-contract/abilities.json' {
  export type AbilityTargetingMode =
    | { kind: 'direction_target' }
    | { kind: 'entity_target' }
    | { kind: 'ground_target' }
    | { kind: 'raycast_strict' }
    | { kind: 'aim_assist' }
    | { kind: 'lock_on'; max_targets: number }
    | { kind: 'self_only' }
    | { kind: 'caster_offset' };

  export type AbilityPreviewShape =
    | { kind: 'none' }
    | { kind: 'capsule'; radius: number; half_height: number }
    | { kind: 'sphere'; radius: number };

  const value: {
    schema_version: number;
    tick_rate: number;
    abilities: Array<{
      ability_id: number;
      name: string;
      base_damage: number;
      damage_type: 'physical' | 'magical' | 'true';
      targeting_mode: AbilityTargetingMode;
      cast_facing_policy: 'preserve_body' | 'face_aim_direction' | 'face_resolved_target';
      max_range: number | null;
      projectile_speed: number | null;
      cooldown_ticks: number;
      linger_ticks: number;
      damage_interval_ticks: number;
      timeline_duration_ticks: number;
      hitbox_spawn_tick: number | null;
      damage_frame_tick: number | null;
      hitbox_remove_tick: number | null;
      lock_on_timeout_ticks: number | null;
      charge_tiers: Array<{ min_ticks: number; damage_mult: number }>;
      preview_shape: AbilityPreviewShape;
      offset: [number, number, number];
    }>;
  };
  export default value;
}

declare module '@dive/client-contract/content-lookup.json' {
  const value: {
    layer_ids: number[];
    layer_name_to_ids: Record<string, number[]>;
    dungeon_template_ids: string[];
    terrain_sets: string[];
  };
  export default value;
}

declare module '@dive/client-contract/map-bundles' {
  export type ShapeMetadata =
    | { kind: 'cuboid'; half_x: number; half_y: number; half_z: number }
    | { kind: 'cylinder'; half_height: number; radius: number }
    | {
        kind: 'heightfield';
        nrows: number;
        ncols: number;
        scale_x: number;
        scale_y: number;
        scale_z: number;
        heights: number[];
      }
    | { kind: 'tri_mesh'; vertices: number[]; indices: number[] };

  export type BundleCollider = {
    collider_id: string;
    position: number[];
    rotation: number[];
    shape: ShapeMetadata;
  };

  export type CollisionMeshRef = {
    terrain_set: string;
    entry_url: string;
    assets: Array<{ url: string; sha256: string; bytes: number }>;
    transform: {
      scale: number;
      offset: number[];
      flip_winding: boolean;
    };
    content_hash: string;
  };

  export type MapBundleEntry = {
    bundle_id: string;
    manifest: {
      schema_version: number;
      bundle_version: string;
      content_hash: string;
      visual_content_hash: string | null;
      bundle_id: string;
      source: {
        kind: 'layer' | 'dungeon';
        name: string;
        layer_id: number | null;
        dungeon_template_id: string | null;
        terrain_set: string | null;
        client_visual: string | null;
      };
      render_meshes: Array<{ url: string; sha256: string; bytes: number }>;
      collider_json: Array<{ url: string; sha256: string; bytes: number }>;
      collision_mesh?: CollisionMeshRef;
      debug_markers: Array<{ kind: string; label: string; position: number[] }>;
    };
    colliders: {
      format_version: number;
      content_hash: string;
      coordinate_system: string;
      source: MapBundleEntry['manifest']['source'];
      colliders: BundleCollider[];
    };
  };

  export const mapBundles: {
    schema_version: number;
    bundles: MapBundleEntry[];
  };
  export default mapBundles;
}

declare module '@dive/client-contract/content-metadata.json' {
  const value: {
    schema_version: number;
    layers: Array<{
      layer_id: number;
      name: string;
      terrain_set: string | null;
      client_visual: string | null;
      spawn_points: number[][];
      geometry: unknown[];
    }>;
    dungeon_templates: Array<{
      template_id: string;
      name: string;
      max_players: number;
      terrain_set: string | null;
      client_visual: string | null;
      spawn_points: number[][];
      exit_points: number[][];
      geometry: unknown[];
      interactables: unknown[];
    }>;
  };
  export default value;
}
