use game_protocol::types::Vec3f;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_ROUTE_ARRIVE_RADIUS: f32 = 0.5;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteDef {
    pub route_id: String,
    pub layer_scope: Option<u32>,
    pub waypoints: Vec<Vec3f>,
    #[serde(default)]
    pub looped: bool,
    #[serde(default = "default_route_arrive_radius")]
    pub arrive_radius: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteFile {
    pub routes: Vec<RouteDef>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteFollowState {
    pub route_id: String,
    pub waypoint_index: usize,
    pub completed: bool,
}

impl RouteFollowState {
    pub fn new(route_id: String) -> Self {
        Self {
            route_id,
            waypoint_index: 0,
            completed: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RouteRegistry {
    routes: HashMap<String, RouteDef>,
}

impl RouteRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_file(file: RouteFile) -> Result<Self, String> {
        let mut registry = Self::new();
        for route in file.routes {
            registry.register(route)?;
        }
        Ok(registry)
    }

    pub fn from_ron(src: &str) -> Result<Self, String> {
        let file: RouteFile = ron::from_str(src).map_err(|err| err.to_string())?;
        Self::from_file(file)
    }

    pub fn register(&mut self, route: RouteDef) -> Result<(), String> {
        validate_route(&route)?;
        if self.routes.contains_key(&route.route_id) {
            return Err(format!("duplicate route_id '{}'", route.route_id));
        }
        self.routes.insert(route.route_id.clone(), route);
        Ok(())
    }

    pub fn get(&self, route_id: &str) -> Option<&RouteDef> {
        self.routes.get(route_id)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

fn validate_route(route: &RouteDef) -> Result<(), String> {
    if route.route_id.trim().is_empty() {
        return Err("route_id must not be empty".to_string());
    }
    if route.waypoints.is_empty() {
        return Err(format!(
            "route '{}' must have at least one waypoint",
            route.route_id
        ));
    }
    if !route.arrive_radius.is_finite() || route.arrive_radius <= 0.0 {
        return Err(format!(
            "route '{}' has invalid arrive_radius {}",
            route.route_id, route.arrive_radius
        ));
    }
    for (index, waypoint) in route.waypoints.iter().enumerate() {
        if !waypoint.x.is_finite() || !waypoint.y.is_finite() || !waypoint.z.is_finite() {
            return Err(format!(
                "route '{}' waypoint {} must be finite",
                route.route_id, index
            ));
        }
    }
    Ok(())
}

fn default_route_arrive_radius() -> f32 {
    DEFAULT_ROUTE_ARRIVE_RADIUS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shipped_routes_ron() {
        let registry = RouteRegistry::from_ron(include_str!("../../../../data/npc_routes.ron"))
            .expect("npc_routes.ron should parse");

        assert!(registry.get("training_patrol_loop").is_some());
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let src = r#"
(
    routes: [
        (route_id: "dup", layer_scope: None, waypoints: [(x: 0.0, y: 1.0, z: 0.0)]),
        (route_id: "dup", layer_scope: None, waypoints: [(x: 1.0, y: 1.0, z: 0.0)]),
    ],
)
"#;

        let err = RouteRegistry::from_ron(src).expect_err("duplicate route IDs should fail");

        assert!(err.contains("duplicate route_id 'dup'"));
    }

    #[test]
    fn rejects_empty_waypoints() {
        let src = r#"
(
    routes: [
        (route_id: "empty", layer_scope: None, waypoints: []),
    ],
)
"#;

        let err = RouteRegistry::from_ron(src).expect_err("empty routes should fail");

        assert!(err.contains("route 'empty' must have at least one waypoint"));
    }
}
