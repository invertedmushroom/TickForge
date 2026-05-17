fn main() {
    env_logger::init();

    #[cfg(feature = "connected")]
    {
        use game_client::client::{self, ClientConfig};

        let args: Vec<String> = std::env::args().collect();
        let test_mode = args.iter().any(|a| a == "--test");
        let multi_test_mode = args.iter().any(|a| a == "--test-multi");

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
