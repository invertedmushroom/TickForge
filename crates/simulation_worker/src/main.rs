use log::info;

fn main() {
    env_logger::init();

    #[cfg(feature = "connected")]
    {
        use simulation_worker::coordinator::{self, CoordinatorConfig};

        let config = CoordinatorConfig {
            uri: std::env::var("STDB_URI").unwrap_or_else(|_| "http://localhost:3000".into()),
            module_name: std::env::var("STDB_MODULE").unwrap_or_else(|_| "jump".into()),
            auth_token: std::env::var("STDB_TOKEN").ok(),
        };

        info!("Starting coordinator → {}:{}", config.uri, config.module_name);
        coordinator::run(config);
    }

    #[cfg(not(feature = "connected"))]
    {
        use simulation_worker::physics::rapier_world::PhysicsWorld;
        use simulation_worker::physics::collision_groups;
        use game_protocol::entity_id::EntityId;
        use game_protocol::tick::TickConfig;
        use rapier3d::math::Vector;

        let tick_config = TickConfig::default_20hz();
        info!(
            "Starting simulation worker — {}Hz ({:.1}ms tick)",
            tick_config.rate_hz,
            tick_config.dt * 1000.0
        );

        let mut world = PhysicsWorld::new(tick_config.dt);
        info!("Physics world initialized");

        // Demo: create a ground plane and a falling ball
        let ground_id = world.add_static_ground(EntityId(1));
        info!("Ground plane created: {:?}", ground_id);

        let ball_id = world.add_dynamic_sphere(
            EntityId(2),
            Vector::new(0.0, 10.0, 0.0),
            0.5,
            1.0,
            collision_groups::player_body_groups(),
        );
        info!("Dynamic ball spawned at y=10: {:?}", ball_id);

        // Run 200 ticks (10 seconds at 20Hz) — ball should fall and rest on ground
        for tick in 0..200 {
            world.step();

            if tick % 20 == 0
                && let Some(pos) = world.get_body_position(ball_id) {
                    info!("Tick {:>3}: ball position = ({:.3}, {:.3}, {:.3})", tick, pos.x, pos.y, pos.z);
                }
        }

        // Final position
        if let Some(pos) = world.get_body_position(ball_id) {
            info!("Final ball position: ({:.3}, {:.3}, {:.3})", pos.x, pos.y, pos.z);
            assert!(pos.y < 1.0, "Ball should have fallen near ground (y≈0.5)");
            assert!(pos.y > 0.0, "Ball should rest on ground, not fall through");
            info!("Physics validation passed — ball fell and rested on ground");
        }

        // Demo: raycast downward from above the ball
        let ray_origin = Vector::new(0.0, 20.0, 0.0);
        let ray_dir = Vector::new(0.0, -1.0, 0.0);
        if let Some(hit) = world.raycast(ray_origin, ray_dir, 100.0) {
            info!("Raycast hit at toi={:.3}, collider={:?}", hit.toi, hit.collider);
        } else {
            info!("Raycast missed (unexpected)");
        }

        info!("Simulation worker demo complete");
    }
}
