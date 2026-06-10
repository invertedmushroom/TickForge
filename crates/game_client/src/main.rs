fn main() {
    env_logger::init();

    #[cfg(feature = "connected")]
    {
        use game_client::client::{self, ClientConfig};

        let args: Vec<String> = std::env::args().collect();
        let test_mode = args.iter().any(|a| a == "--test");
        let multi_test_mode = args.iter().any(|a| a == "--test-multi");
        let loot_smoke_mode = args.iter().any(|a| a == "--loot-smoke");
        let claim_loot_args = args.iter().position(|a| a == "--claim-loot");

        let config = ClientConfig {
            uri: std::env::var("STDB_URI").unwrap_or_else(|_| "http://localhost:3000".into()),
            module_name: std::env::var("STDB_MODULE").unwrap_or_else(|_| "tickforge".into()),
            auth_token: std::env::var("STDB_TOKEN").ok(),
        };

        if multi_test_mode {
            log::info!(
                "Running multi-client tests against {}:{}",
                config.uri,
                config.module_name
            );
            let exit_code = game_client::multi_client_test::run_tests(config);
            std::process::exit(exit_code);
        }

        if let Some(index) = claim_loot_args {
            let Some(loot_pile_id) = args.get(index + 1).and_then(|arg| arg.parse::<u64>().ok())
            else {
                eprintln!("Usage: game_client --claim-loot <loot_pile_id> <item_id> <target_slot>");
                std::process::exit(2);
            };
            let Some(item_id) = args.get(index + 2).and_then(|arg| arg.parse::<u32>().ok()) else {
                eprintln!("Usage: game_client --claim-loot <loot_pile_id> <item_id> <target_slot>");
                std::process::exit(2);
            };
            let Some(target_slot) = args.get(index + 3).and_then(|arg| arg.parse::<u32>().ok())
            else {
                eprintln!("Usage: game_client --claim-loot <loot_pile_id> <item_id> <target_slot>");
                std::process::exit(2);
            };

            log::info!(
                "Claiming loot against {}:{} pile={} item={} slot={}",
                config.uri,
                config.module_name,
                loot_pile_id,
                item_id,
                target_slot
            );
            let exit_code =
                game_client::loot_claim::claim_loot(config, loot_pile_id, item_id, target_slot);
            std::process::exit(exit_code);
        }

        if loot_smoke_mode {
            log::info!(
                "Running loot smoke against {}:{}",
                config.uri,
                config.module_name
            );
            let exit_code = game_client::loot_claim::run_loot_smoke(config);
            std::process::exit(exit_code);
        }

        if test_mode {
            log::info!(
                "Running integration tests against {}:{}",
                config.uri,
                config.module_name
            );
            let exit_code = game_client::smoke_test::run_tests(config);
            std::process::exit(exit_code);
        }

        log::info!("Connecting to {}:{}", config.uri, config.module_name);
        client::run(config);
    }

    #[cfg(not(feature = "connected"))]
    {
        eprintln!("Build with --features connected to use the SDK client.");
        eprintln!("  cargo run -p game_client --features connected");
        std::process::exit(1);
    }
}
