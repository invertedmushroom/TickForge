//! Authored waypoint/portal navigation graph and bounded A*.
//!
//! This extends the route system: where [`super::routes`] stores fixed,
//! pre-ordered patrol waypoint lists, this module stores a graph of authored
//! nodes and directed links per layer and runs a bounded A* over it to produce a
//! short waypoint path between two nodes. Physical movement still goes through
//! the worker's KCC; A* only proposes a route.
//!
//! ## Node-identity invariant
//!
//! The path cache keys routes on `(layer, graph_version, start, goal,
//! capability)` with no `graph_id`. That is collision-free iff a
//! [`NavNodeId`] is unique within a `(layer, graph_version)`. This module
//! enforces that structurally: there is exactly one [`NavGraph`] per layer
//! (the registry is keyed by layer), and node ids are unique within a graph.
//! Region/dungeon sub-areas on the same layer are authored as *disjoint node
//! subsets of that one per-layer graph*, not as separate graphs with their own
//! local numbering — so two authored areas can never reuse the same id on the
//! same layer and the cache key needs no `graph_id` discriminator. If a future
//! design needs multiple independent graphs per layer, add `graph_id` to both
//! the graph and the cache key together.
//!
//! ## Boundedness
//!
//! Both the per-search work (`max_nodes_expanded`) and the result length
//! (`max_waypoints`) are capped, so a pathological graph can never make one A*
//! call unbounded — this is the algorithm-level complement to the per-tick
//! search budget. Link costs and the heuristic are 3D Euclidean
//! distances, so the heuristic is admissible and A* returns optimal paths.
//!
//! Pure `game_core`: no DB access, no dense-store mutation, no worker types.

use game_protocol::types::Vec3f;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// Default ceiling on nodes expanded (popped from the open set) in one A* call.
pub const DEFAULT_MAX_NODES_EXPANDED: usize = 256;

/// Default ceiling on the number of waypoints in a returned path. A search whose
/// optimal path would exceed this returns `None` (caller falls back to steering)
/// rather than a truncated, invalid route.
pub const DEFAULT_MAX_WAYPOINTS: usize = 64;

/// Identifier of a node in a [`NavGraph`]. Unique within a graph, hence unique
/// within a `(layer, graph_version)` — see the module-level node-identity
/// invariant. Authored ids are opaque `u32`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NavNodeId(pub u32);

/// One authored graph node: an id and a world position.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavNode {
    pub id: NavNodeId,
    pub position: Vec3f,
}

/// One authored directed link between two nodes.
///
/// `cost` is optional in authoring: when omitted it defaults to the 3D Euclidean
/// distance between the endpoints (the common case). Author an explicit `cost`
/// only to bias a link (e.g. make a portal cheaper/pricier than its geometric
/// length). Bidirectional connections are authored via `bidirectional: true`,
/// which expands to a link in each direction at load time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavLink {
    pub from: NavNodeId,
    pub to: NavNodeId,
    #[serde(default)]
    pub cost: Option<f32>,
    #[serde(default)]
    pub bidirectional: bool,
}

/// A resolved path through the graph: the ordered node ids and their world
/// positions, plus the `(layer, graph_version)` the search ran against so a
/// consumer can reject a path that outlived its graph or crossed layers.
#[derive(Clone, Debug, PartialEq)]
pub struct NavPath {
    pub layer: u32,
    pub graph_version: u64,
    pub nodes: Vec<NavNodeId>,
    pub waypoints: Vec<Vec3f>,
}

/// An authored navigation graph for one layer. Holds node positions and a
/// directed adjacency list with per-edge cost; one graph per layer (registry is
/// keyed by layer), so node ids are unique within a `(layer, graph_version)`.
#[derive(Clone, Debug, PartialEq)]
pub struct NavGraph {
    layer: u32,
    graph_version: u64,
    positions: HashMap<NavNodeId, Vec3f>,
    /// `from -> [(to, cost)]`.
    adjacency: HashMap<NavNodeId, Vec<(NavNodeId, f32)>>,
}

impl NavGraph {
    /// Build a graph from authored nodes and links, validating the
    /// node-identity invariant and link referential integrity. Bidirectional
    /// links expand to two directed edges; omitted costs default to the 3D
    /// Euclidean distance between endpoints.
    pub fn build(
        layer: u32,
        graph_version: u64,
        nodes: &[NavNode],
        links: &[NavLink],
    ) -> Result<Self, String> {
        let mut positions: HashMap<NavNodeId, Vec3f> = HashMap::with_capacity(nodes.len());
        for node in nodes {
            if !is_finite(node.position) {
                return Err(format!("node {} has a non-finite position", node.id.0));
            }
            if positions.insert(node.id, node.position).is_some() {
                return Err(format!(
                    "duplicate node id {} in layer {layer} graph (node ids must be \
                     unique within a layer/graph_version)",
                    node.id.0
                ));
            }
        }

        let mut adjacency: HashMap<NavNodeId, Vec<(NavNodeId, f32)>> = HashMap::new();
        let mut add_edge = |from: NavNodeId, to: NavNodeId, cost: Option<f32>| -> Result<(), String> {
            let from_pos = positions
                .get(&from)
                .ok_or_else(|| format!("link references unknown node {}", from.0))?;
            let to_pos = positions
                .get(&to)
                .ok_or_else(|| format!("link references unknown node {}", to.0))?;
            if from == to {
                return Err(format!("self-link on node {} is not allowed", from.0));
            }
            let resolved = match cost {
                Some(c) if c.is_finite() && c > 0.0 => c,
                Some(c) => {
                    return Err(format!(
                        "link {}->{} has invalid cost {c}",
                        from.0, to.0
                    ));
                }
                None => euclidean(*from_pos, *to_pos),
            };
            adjacency.entry(from).or_default().push((to, resolved));
            Ok(())
        };

        for link in links {
            add_edge(link.from, link.to, link.cost)?;
            if link.bidirectional {
                add_edge(link.to, link.from, link.cost)?;
            }
        }

        Ok(Self {
            layer,
            graph_version,
            positions,
            adjacency,
        })
    }

    pub fn layer(&self) -> u32 {
        self.layer
    }

    pub fn graph_version(&self) -> u64 {
        self.graph_version
    }

    pub fn node_count(&self) -> usize {
        self.positions.len()
    }

    pub fn position_of(&self, node: NavNodeId) -> Option<Vec3f> {
        self.positions.get(&node).copied()
    }

    pub fn contains(&self, node: NavNodeId) -> bool {
        self.positions.contains_key(&node)
    }

    /// Resolve the graph node nearest to `pos` in the XZ plane (the worker moves
    /// on XZ; height is ignored to match KCC steering). Returns `None` only for
    /// an empty graph. Ties break on the lower [`NavNodeId`] for determinism.
    pub fn nearest_node(&self, pos: Vec3f) -> Option<NavNodeId> {
        let mut best: Option<(NavNodeId, f32)> = None;
        for (&id, &node_pos) in &self.positions {
            let dx = node_pos.x - pos.x;
            let dz = node_pos.z - pos.z;
            let dist_sq = dx * dx + dz * dz;
            let replace = match best {
                None => true,
                Some((best_id, best_sq)) => {
                    dist_sq < best_sq || (dist_sq == best_sq && id < best_id)
                }
            };
            if replace {
                best = Some((id, dist_sq));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Run bounded A* from `start` to `goal`.
    ///
    /// Returns `None` when: either endpoint is unknown, no route exists, the
    /// search expands more than `max_nodes_expanded` nodes, or the optimal path
    /// would exceed `max_waypoints`. In every `None` case the caller falls back
    /// to existing steering — A* never returns a truncated or partial path.
    pub fn find_path(
        &self,
        start: NavNodeId,
        goal: NavNodeId,
        max_nodes_expanded: usize,
        max_waypoints: usize,
    ) -> Option<NavPath> {
        if !self.contains(start) || !self.contains(goal) {
            return None;
        }
        if start == goal {
            let position = self.positions[&start];
            return Some(NavPath {
                layer: self.layer,
                graph_version: self.graph_version,
                nodes: vec![start],
                waypoints: vec![position],
            });
        }

        let goal_pos = self.positions[&goal];

        let mut g_score: HashMap<NavNodeId, f32> = HashMap::new();
        let mut came_from: HashMap<NavNodeId, NavNodeId> = HashMap::new();
        let mut open = BinaryHeap::new();
        let mut closed: HashSet<NavNodeId> = HashSet::new();

        g_score.insert(start, 0.0);
        open.push(OpenEntry {
            f_score: euclidean(self.positions[&start], goal_pos),
            node: start,
        });

        let mut expanded = 0usize;
        while let Some(OpenEntry { node: current, .. }) = open.pop() {
            if current == goal {
                return self.reconstruct(start, goal, &came_from, max_waypoints);
            }
            // Skip stale heap entries (a better path to `current` was already
            // finalized) so they don't count against the expansion budget.
            if !closed.insert(current) {
                continue;
            }
            expanded += 1;
            if expanded > max_nodes_expanded {
                return None;
            }

            let current_g = g_score.get(&current).copied().unwrap_or(f32::INFINITY);
            let Some(neighbors) = self.adjacency.get(&current) else {
                continue;
            };
            for &(next, cost) in neighbors {
                if closed.contains(&next) {
                    continue;
                }
                let tentative = current_g + cost;
                if tentative < g_score.get(&next).copied().unwrap_or(f32::INFINITY) {
                    came_from.insert(next, current);
                    g_score.insert(next, tentative);
                    let f = tentative + euclidean(self.positions[&next], goal_pos);
                    open.push(OpenEntry {
                        f_score: f,
                        node: next,
                    });
                }
            }
        }

        None
    }

    fn reconstruct(
        &self,
        start: NavNodeId,
        goal: NavNodeId,
        came_from: &HashMap<NavNodeId, NavNodeId>,
        max_waypoints: usize,
    ) -> Option<NavPath> {
        let mut nodes = vec![goal];
        let mut current = goal;
        while current != start {
            current = *came_from.get(&current)?;
            nodes.push(current);
            if nodes.len() > max_waypoints {
                // Optimal path is longer than the caller will accept.
                return None;
            }
        }
        nodes.reverse();
        let waypoints = nodes.iter().map(|n| self.positions[n]).collect();
        Some(NavPath {
            layer: self.layer,
            graph_version: self.graph_version,
            nodes,
            waypoints,
        })
    }
}

/// Registry of authored navigation graphs, **keyed by layer**. The keying is the
/// structural guarantee behind the node-identity invariant: one graph per layer
/// means node ids are unique within a `(layer, graph_version)`.
#[derive(Clone, Debug, Default)]
pub struct NavGraphRegistry {
    graphs: HashMap<u32, NavGraph>,
}

impl NavGraphRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a [`NavGraphFile`] from RON and build every authored graph.
    pub fn from_ron(src: &str) -> Result<Self, String> {
        let file: NavGraphFile = ron::from_str(src).map_err(|err| err.to_string())?;
        Self::from_file(file)
    }

    pub fn from_file(file: NavGraphFile) -> Result<Self, String> {
        let mut registry = Self::new();
        for graph in file.graphs {
            registry.register(graph)?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, def: NavGraphDef) -> Result<(), String> {
        if self.graphs.contains_key(&def.layer) {
            return Err(format!(
                "duplicate nav graph for layer {} (one graph per layer)",
                def.layer
            ));
        }
        let graph = NavGraph::build(def.layer, def.graph_version, &def.nodes, &def.links)?;
        self.graphs.insert(def.layer, graph);
        Ok(())
    }

    pub fn graph_for_layer(&self, layer: u32) -> Option<&NavGraph> {
        self.graphs.get(&layer)
    }

    pub fn len(&self) -> usize {
        self.graphs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.graphs.is_empty()
    }
}

/// Authoring file: a list of per-layer graph definitions (RON).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavGraphFile {
    pub graphs: Vec<NavGraphDef>,
}

/// One authored per-layer graph: its layer, version, nodes, and links.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavGraphDef {
    pub layer: u32,
    pub graph_version: u64,
    pub nodes: Vec<NavNode>,
    pub links: Vec<NavLink>,
}

/// Open-set entry ordered by ascending `f_score` (min-heap via reversed `Ord`).
struct OpenEntry {
    f_score: f32,
    node: NavNodeId,
}

impl PartialEq for OpenEntry {
    fn eq(&self, other: &Self) -> bool {
        self.f_score == other.f_score && self.node == other.node
    }
}

impl Eq for OpenEntry {}

impl PartialOrd for OpenEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OpenEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse the f_score comparison so `BinaryHeap` (a max-heap) yields the
        // lowest f_score first. `total_cmp` keeps NaN from breaking the heap.
        other
            .f_score
            .total_cmp(&self.f_score)
            .then_with(|| self.node.cmp(&other.node))
    }
}

fn euclidean(a: Vec3f, b: Vec3f) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn is_finite(p: Vec3f) -> bool {
    p.x.is_finite() && p.y.is_finite() && p.z.is_finite()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, x: f32, z: f32) -> NavNode {
        NavNode {
            id: NavNodeId(id),
            position: Vec3f { x, y: 0.0, z },
        }
    }

    fn link(from: u32, to: u32) -> NavLink {
        NavLink {
            from: NavNodeId(from),
            to: NavNodeId(to),
            cost: None,
            bidirectional: true,
        }
    }

    /// A diamond graph: 0 -> {1,2} -> 3. The lower-cost arm (via node 2) must be
    /// chosen, and the returned waypoints must match the node positions.
    fn diamond() -> NavGraph {
        let nodes = [
            node(0, 0.0, 0.0),
            node(1, 1.0, 2.0), // long arm
            node(2, 1.0, 0.0), // short arm
            node(3, 2.0, 0.0),
        ];
        let links = [link(0, 1), link(1, 3), link(0, 2), link(2, 3)];
        NavGraph::build(0, 1, &nodes, &links).expect("diamond builds")
    }

    #[test]
    fn a_star_finds_lowest_cost_path() {
        let graph = diamond();
        let path = graph
            .find_path(NavNodeId(0), NavNodeId(3), DEFAULT_MAX_NODES_EXPANDED, DEFAULT_MAX_WAYPOINTS)
            .expect("a path exists");

        assert_eq!(
            path.nodes,
            vec![NavNodeId(0), NavNodeId(2), NavNodeId(3)],
            "A* must take the shorter arm through node 2"
        );
        assert_eq!(path.layer, 0);
        assert_eq!(path.graph_version, 1);
        assert_eq!(
            path.waypoints,
            vec![
                Vec3f { x: 0.0, y: 0.0, z: 0.0 },
                Vec3f { x: 1.0, y: 0.0, z: 0.0 },
                Vec3f { x: 2.0, y: 0.0, z: 0.0 },
            ]
        );
    }

    #[test]
    fn start_equals_goal_is_a_single_node_path() {
        let graph = diamond();
        let path = graph
            .find_path(NavNodeId(2), NavNodeId(2), DEFAULT_MAX_NODES_EXPANDED, DEFAULT_MAX_WAYPOINTS)
            .expect("trivial path");
        assert_eq!(path.nodes, vec![NavNodeId(2)]);
        assert_eq!(path.waypoints.len(), 1);
    }

    #[test]
    fn unknown_endpoint_returns_none() {
        let graph = diamond();
        assert!(graph
            .find_path(NavNodeId(0), NavNodeId(99), 256, 64)
            .is_none());
        assert!(graph
            .find_path(NavNodeId(99), NavNodeId(3), 256, 64)
            .is_none());
    }

    #[test]
    fn disconnected_goal_returns_none() {
        // Node 5 is isolated (no links touch it).
        let nodes = [node(0, 0.0, 0.0), node(1, 1.0, 0.0), node(5, 9.0, 9.0)];
        let links = [link(0, 1)];
        let graph = NavGraph::build(0, 1, &nodes, &links).unwrap();
        assert!(graph.find_path(NavNodeId(0), NavNodeId(5), 256, 64).is_none());
    }

    #[test]
    fn directed_link_is_not_traversable_backwards() {
        // One-way 0 -> 1 only.
        let nodes = [node(0, 0.0, 0.0), node(1, 1.0, 0.0)];
        let links = [NavLink {
            from: NavNodeId(0),
            to: NavNodeId(1),
            cost: None,
            bidirectional: false,
        }];
        let graph = NavGraph::build(0, 1, &nodes, &links).unwrap();
        assert!(graph.find_path(NavNodeId(0), NavNodeId(1), 256, 64).is_some());
        assert!(
            graph.find_path(NavNodeId(1), NavNodeId(0), 256, 64).is_none(),
            "a one-way link must not be walkable in reverse"
        );
    }

    #[test]
    fn node_expansion_cap_bounds_the_search() {
        // A long line 0-1-2-...-10; reaching node 10 needs >2 expansions.
        let nodes: Vec<NavNode> = (0..=10).map(|i| node(i, i as f32, 0.0)).collect();
        let links: Vec<NavLink> = (0..10).map(|i| link(i, i + 1)).collect();
        let graph = NavGraph::build(0, 1, &nodes, &links).unwrap();

        // Generous cap → found.
        assert!(graph.find_path(NavNodeId(0), NavNodeId(10), 256, 64).is_some());
        // Tiny expansion cap → bailout, None (not a partial path).
        assert!(
            graph.find_path(NavNodeId(0), NavNodeId(10), 2, 64).is_none(),
            "search must abort once the expansion cap is exceeded"
        );
    }

    #[test]
    fn waypoint_cap_rejects_overlong_paths() {
        let nodes: Vec<NavNode> = (0..=10).map(|i| node(i, i as f32, 0.0)).collect();
        let links: Vec<NavLink> = (0..10).map(|i| link(i, i + 1)).collect();
        let graph = NavGraph::build(0, 1, &nodes, &links).unwrap();

        // The optimal path is 11 nodes; a cap of 4 must reject it as None.
        assert!(
            graph.find_path(NavNodeId(0), NavNodeId(10), 256, 4).is_none(),
            "a path longer than max_waypoints must be rejected, not truncated"
        );
        // A cap of exactly 11 admits it.
        assert!(graph.find_path(NavNodeId(0), NavNodeId(10), 256, 11).is_some());
    }

    #[test]
    fn build_rejects_duplicate_node_ids() {
        let nodes = [node(7, 0.0, 0.0), node(7, 1.0, 1.0)];
        let err = NavGraph::build(0, 1, &nodes, &[]).expect_err("dup ids rejected");
        assert!(err.contains("duplicate node id 7"), "got: {err}");
    }

    #[test]
    fn build_rejects_dangling_link() {
        let nodes = [node(0, 0.0, 0.0)];
        let links = [NavLink {
            from: NavNodeId(0),
            to: NavNodeId(1), // node 1 does not exist
            cost: None,
            bidirectional: false,
        }];
        let err = NavGraph::build(0, 1, &nodes, &links).expect_err("dangling link rejected");
        assert!(err.contains("unknown node 1"), "got: {err}");
    }

    #[test]
    fn build_rejects_self_link_and_bad_cost() {
        let nodes = [node(0, 0.0, 0.0), node(1, 1.0, 0.0)];
        let self_link = [NavLink {
            from: NavNodeId(0),
            to: NavNodeId(0),
            cost: None,
            bidirectional: false,
        }];
        assert!(NavGraph::build(0, 1, &nodes, &self_link)
            .unwrap_err()
            .contains("self-link"));

        let bad_cost = [NavLink {
            from: NavNodeId(0),
            to: NavNodeId(1),
            cost: Some(-1.0),
            bidirectional: false,
        }];
        assert!(NavGraph::build(0, 1, &nodes, &bad_cost)
            .unwrap_err()
            .contains("invalid cost"));
    }

    #[test]
    fn explicit_cost_overrides_geometric_distance() {
        // Geometrically the direct arm 0->3 (dist 10) is longer than 0->1->3
        // (5+5). But author 0->3 with a cheap explicit cost so it wins.
        let nodes = [
            node(0, 0.0, 0.0),
            node(1, 5.0, 0.0),
            node(3, 10.0, 0.0),
        ];
        let links = [
            NavLink { from: NavNodeId(0), to: NavNodeId(1), cost: None, bidirectional: false },
            NavLink { from: NavNodeId(1), to: NavNodeId(3), cost: None, bidirectional: false },
            NavLink { from: NavNodeId(0), to: NavNodeId(3), cost: Some(1.0), bidirectional: false },
        ];
        let graph = NavGraph::build(0, 1, &nodes, &links).unwrap();
        let path = graph.find_path(NavNodeId(0), NavNodeId(3), 256, 64).unwrap();
        assert_eq!(
            path.nodes,
            vec![NavNodeId(0), NavNodeId(3)],
            "the cheap explicit-cost portal must beat the geometric two-hop"
        );
    }

    #[test]
    fn registry_is_keyed_by_layer_and_rejects_duplicates() {
        let mut registry = NavGraphRegistry::new();
        registry
            .register(NavGraphDef {
                layer: 0,
                graph_version: 1,
                nodes: vec![node(0, 0.0, 0.0), node(1, 1.0, 0.0)],
                links: vec![link(0, 1)],
            })
            .unwrap();
        // A different layer is fine and may reuse node ids without collision.
        registry
            .register(NavGraphDef {
                layer: 1,
                graph_version: 1,
                nodes: vec![node(0, 5.0, 5.0), node(1, 6.0, 5.0)],
                links: vec![link(0, 1)],
            })
            .unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.graph_for_layer(0).is_some());
        assert!(registry.graph_for_layer(1).is_some());
        assert!(registry.graph_for_layer(2).is_none());

        // A second graph for an existing layer is rejected.
        let err = registry
            .register(NavGraphDef {
                layer: 0,
                graph_version: 2,
                nodes: vec![node(0, 0.0, 0.0)],
                links: vec![],
            })
            .expect_err("duplicate layer rejected");
        assert!(err.contains("duplicate nav graph for layer 0"));
    }

    #[test]
    fn nearest_node_picks_closest_in_xz_with_deterministic_tiebreak() {
        // Two nodes equidistant in XZ from the query; lower id must win.
        let nodes = [node(2, 1.0, 0.0), node(1, -1.0, 0.0), node(3, 5.0, 5.0)];
        let graph = NavGraph::build(0, 1, &nodes, &[]).unwrap();
        let nearest = graph.nearest_node(Vec3f { x: 0.0, y: 0.0, z: 0.0 }).unwrap();
        assert_eq!(nearest, NavNodeId(1), "equidistant tie breaks on lower id");

        let nearest = graph.nearest_node(Vec3f { x: 4.0, y: 0.0, z: 4.0 }).unwrap();
        assert_eq!(nearest, NavNodeId(3));

        // Empty graph yields None.
        let empty = NavGraph::build(0, 1, &[], &[]).unwrap();
        assert!(empty.nearest_node(Vec3f::ZERO).is_none());
    }

    #[test]
    fn round_trips_through_ron() {
        let src = r#"
(
    graphs: [
        (
            layer: 0,
            graph_version: 1,
            nodes: [
                (id: (0), position: (x: 0.0, y: 0.0, z: 0.0)),
                (id: (1), position: (x: 1.0, y: 0.0, z: 0.0)),
                (id: (2), position: (x: 2.0, y: 0.0, z: 0.0)),
            ],
            links: [
                (from: (0), to: (1), bidirectional: true),
                (from: (1), to: (2), bidirectional: true),
            ],
        ),
    ],
)
"#;
        let registry = NavGraphRegistry::from_ron(src).expect("parses");
        let graph = registry.graph_for_layer(0).expect("layer 0 graph");
        assert_eq!(graph.node_count(), 3);
        let path = graph.find_path(NavNodeId(0), NavNodeId(2), 256, 64).unwrap();
        assert_eq!(path.nodes, vec![NavNodeId(0), NavNodeId(1), NavNodeId(2)]);
    }
}
