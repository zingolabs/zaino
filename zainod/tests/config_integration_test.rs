//! Integration test demonstrating programmatic configuration construction.
//!
//! This test shows how external consumers can easily build IndexerConfig
//! for integration tests and different deployment scenarios.

use std::path::PathBuf;
use tempfile::TempDir;
use zaino_commons::config::{
    AuthMethod, BackendType, CacheConfig, CookieAuth, DatabaseConfig, Network, ServiceConfig,
    ValidatorConfig,
};
use zainodlib::config::{DebugConfig, IndexerConfig, ServerConfig, StorageConfig};

#[test]
fn test_programmatic_config_construction() {
    // Example 1: Integration test configuration
    let temp_dir = TempDir::new().unwrap();

    let integration_test_config = IndexerConfig {
        backend: BackendType::Fetch,
        network: Network::Regtest,
        server: ServerConfig {
            enable_json_server: true,
            json_rpc_listen_address: "127.0.0.1:0".parse().unwrap(), // random port
            cookie: CookieAuth::Disabled,                            // no auth needed for tests
            grpc_listen_address: "127.0.0.1:0".parse().unwrap(),
            grpc_tls: false, // no TLS for local tests
            tls_cert_path: None,
            tls_key_path: None,
        },
        validator: ValidatorConfig {
            config: zaino_commons::config::ZainoStateConfig::default(),
            rpc_address: "127.0.0.1:18232".parse().unwrap(),
            indexer_rpc_address: "127.0.0.1:18230".parse().unwrap(),
            auth: AuthMethod::default(),
        },
        service: ServiceConfig {
            timeout: 10,      // shorter timeout for tests
            channel_size: 16, // smaller channels for tests
        },
        storage: StorageConfig {
            cache: CacheConfig {
                capacity: Some(100), // small cache for tests
                shard_amount: Some(4),
            },
            zaino_database: DatabaseConfig {
                path: temp_dir.path().join("zaino_test.db"),
                size: Some(1), // 1GB max for tests
            },
            zebra_database: DatabaseConfig {
                path: temp_dir.path().join("zebra_test.db"),
                size: None,
            },
        },
        debug: DebugConfig {
            no_sync: true, // disable sync for faster tests
            no_db: false,  // still want DB functionality
            slow_sync: false,
        },
    };

    // Verify the config is valid
    integration_test_config.check_config().unwrap();
    assert_eq!(integration_test_config.network, Network::Regtest);
    assert_eq!(integration_test_config.backend, BackendType::Fetch);
    assert!(integration_test_config.debug.no_sync);
}

#[test]
fn test_production_config_construction() {
    // Example 2: Production configuration with security
    let cookie_path = PathBuf::from("/var/lib/zaino/cookie");

    let production_config = IndexerConfig {
        backend: BackendType::State,
        network: Network::Mainnet,
        server: ServerConfig {
            enable_json_server: true,
            json_rpc_listen_address: "0.0.0.0:8237".parse().unwrap(),
            cookie: CookieAuth::Enabled {
                path: cookie_path.clone(),
            },
            grpc_listen_address: "0.0.0.0:8137".parse().unwrap(),
            grpc_tls: true,
            tls_cert_path: Some("/etc/ssl/certs/zaino.crt".to_string()),
            tls_key_path: Some("/etc/ssl/private/zaino.key".to_string()),
        },
        validator: ValidatorConfig {
            config: zaino_commons::config::ZainoStateConfig {
                cache_dir: "/var/lib/zebra".into(),
                ephemeral: false,
                delete_old_database: false,
                debug_stop_at_height: None,
                debug_validity_check_interval: None,
            },
            rpc_address: "127.0.0.1:8232".parse().unwrap(),
            indexer_rpc_address: "127.0.0.1:8983".parse().unwrap(),
            auth: AuthMethod::default(),
        },
        service: ServiceConfig {
            timeout: 60,       // longer timeout for production
            channel_size: 128, // larger channels for production
        },
        storage: StorageConfig {
            cache: CacheConfig {
                capacity: Some(10000), // large cache for production
                shard_amount: Some(16),
            },
            zaino_database: DatabaseConfig {
                path: "/var/lib/zaino/data".into(),
                size: Some(100), // 100GB max
            },
            zebra_database: DatabaseConfig {
                path: "/var/lib/zebra/state".into(),
                size: Some(500), // 500GB max
            },
        },
        debug: DebugConfig {
            no_sync: false, // enable sync in production
            no_db: false,   // enable DB in production
            slow_sync: false,
        },
    };

    // Verify the structure
    assert_eq!(production_config.network, Network::Mainnet);
    assert_eq!(production_config.backend, BackendType::State);
    assert!(production_config.server.grpc_tls);

    // Verify cookie auth is enabled
    match production_config.server.cookie {
        CookieAuth::Enabled { path } => assert_eq!(path, cookie_path),
        CookieAuth::Disabled => panic!("Expected cookie auth to be enabled"),
    }
}

#[test]
fn test_config_defaults() {
    // Example 3: Using defaults with minimal customization
    let minimal_config = IndexerConfig {
        network: Network::Testnet,
        debug: DebugConfig {
            no_sync: true, // override default for testing
            ..Default::default()
        },
        ..Default::default()
    };

    assert_eq!(minimal_config.backend, BackendType::Fetch); // default
    assert_eq!(minimal_config.network, Network::Testnet);
    assert!(minimal_config.debug.no_sync); // overridden
    assert!(!minimal_config.debug.no_db); // default
}

#[test]
fn test_serde_roundtrip() {
    // Example 4: Demonstrate TOML serialization/deserialization
    let original_config = IndexerConfig {
        backend: BackendType::Fetch,
        network: Network::Regtest,
        server: ServerConfig {
            enable_json_server: true,
            cookie: CookieAuth::Enabled {
                path: "/tmp/test.cookie".into(),
            },
            ..Default::default()
        },
        ..Default::default()
    };

    // Serialize to TOML
    let toml_string = toml::to_string(&original_config).unwrap();
    println!("TOML configuration:\n{}", toml_string);

    // Deserialize back
    let deserialized_config: IndexerConfig = toml::from_str(&toml_string).unwrap();

    // Verify they match
    assert_eq!(original_config.backend, deserialized_config.backend);
    assert_eq!(original_config.network, deserialized_config.network);
    assert_eq!(
        original_config.server.enable_json_server,
        deserialized_config.server.enable_json_server
    );
}

#[test]
fn test_network_enum_functionality() {
    // Example 5: Demonstrate Network enum benefits
    let configs = [
        (Network::Mainnet, "mainnet"),
        (Network::Testnet, "testnet"),
        (Network::Regtest, "regtest"),
    ];

    for (network, expected_name) in configs {
        let config = IndexerConfig {
            network,
            ..Default::default()
        };

        // Network enum provides type safety
        let _zebra_network: zebra_chain::parameters::Network = config.network.into();

        // Can be serialized to string names
        let serialized = serde_json::to_string(&config.network).unwrap();
        assert!(serialized.contains(expected_name));

        // No string parsing errors possible
        assert_eq!(config.network, network);
    }
}

#[test]
fn test_toml_file_loading() {
    // Example 6: Test loading real TOML files
    let test_cases = [
        ("minimal.toml", Network::Testnet, BackendType::Fetch, false),
        (
            "development.toml",
            Network::Regtest,
            BackendType::Fetch,
            true,
        ),
        (
            "production.toml",
            Network::Mainnet,
            BackendType::State,
            true,
        ),
        (
            "edge_cases.toml",
            Network::Testnet,
            BackendType::Fetch,
            false,
        ),
    ];

    for (filename, expected_network, expected_backend, expected_json_server) in test_cases {
        let toml_path = format!("tests/data/{}", filename);
        let toml_content = std::fs::read_to_string(&toml_path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", toml_path, e));

        // Parse TOML directly with serde
        let config: IndexerConfig = toml::from_str(&toml_content)
            .unwrap_or_else(|e| panic!("Failed to parse {} as TOML: {}", filename, e));

        // Verify key fields
        assert_eq!(
            config.network, expected_network,
            "Network mismatch in {}",
            filename
        );
        assert_eq!(
            config.backend, expected_backend,
            "Backend mismatch in {}",
            filename
        );
        assert_eq!(
            config.server.enable_json_server, expected_json_server,
            "JSON server mismatch in {}",
            filename
        );

        // Verify config is valid
        config
            .check_config()
            .unwrap_or_else(|e| panic!("Config validation failed for {}: {}", filename, e));

        println!("✓ Successfully loaded and validated {}", filename);
    }
}

#[test]
fn test_toml_round_trip_fidelity() {
    // Example 7: Test TOML → Rust → TOML → Rust round-trip fidelity
    let test_files = [
        "minimal.toml",
        "development.toml",
        "production.toml",
        "edge_cases.toml",
    ];

    for filename in test_files {
        let toml_path = format!("tests/data/{}", filename);
        let original_toml = std::fs::read_to_string(&toml_path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", toml_path, e));

        // TOML → Rust
        let config1: IndexerConfig = toml::from_str(&original_toml)
            .unwrap_or_else(|e| panic!("Failed to parse {} as TOML: {}", filename, e));

        // Rust → TOML
        let regenerated_toml = toml::to_string_pretty(&config1)
            .unwrap_or_else(|e| panic!("Failed to serialize {} to TOML: {}", filename, e));

        // TOML → Rust (again)
        let config2: IndexerConfig = toml::from_str(&regenerated_toml).unwrap_or_else(|e| {
            panic!(
                "Failed to re-parse regenerated TOML for {}: {}",
                filename, e
            )
        });

        // Compare critical fields (not exact TOML text, as formatting may differ)
        assert_eq!(
            config1.backend, config2.backend,
            "Backend differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.network, config2.network,
            "Network differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.server.enable_json_server, config2.server.enable_json_server,
            "JSON server differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.server.json_rpc_listen_address, config2.server.json_rpc_listen_address,
            "JSON RPC address differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.server.grpc_listen_address, config2.server.grpc_listen_address,
            "gRPC address differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.validator.rpc_address, config2.validator.rpc_address,
            "Validator address differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.service.timeout, config2.service.timeout,
            "Service timeout differs after round-trip in {}",
            filename
        );
        assert_eq!(
            config1.debug.no_sync, config2.debug.no_sync,
            "Debug no_sync differs after round-trip in {}",
            filename
        );

        // Test cookie auth round-trip
        match (&config1.server.cookie, &config2.server.cookie) {
            (CookieAuth::Enabled { path: p1 }, CookieAuth::Enabled { path: p2 }) => {
                assert_eq!(
                    p1, p2,
                    "Server cookie path differs after round-trip in {}",
                    filename
                );
            }
            (CookieAuth::Disabled, CookieAuth::Disabled) => {}
            _ => panic!(
                "Server cookie auth type differs after round-trip in {}",
                filename
            ),
        }

        println!("✓ Round-trip fidelity verified for {}", filename);
    }
}

#[test]
fn test_figment_integration() {
    // Example 8: Test the actual Figment loading pipeline used by zaino
    use figment::{
        providers::{Format, Serialized, Toml},
        Figment,
    };

    let toml_path = "tests/data/development.toml";

    // Test Figment loading (same as load_config function)
    let figment = Figment::new()
        .merge(Serialized::defaults(IndexerConfig::default()))
        .merge(Toml::file(toml_path))
        .merge(figment::providers::Env::prefixed("ZAINO_TEST_"));

    let config: IndexerConfig = figment
        .extract()
        .unwrap_or_else(|e| panic!("Figment failed to extract config: {}", e));

    // Verify it loaded correctly
    assert_eq!(config.network, Network::Regtest);
    assert_eq!(config.backend, BackendType::Fetch);
    assert!(config.server.enable_json_server);
    assert!(config.debug.no_sync);

    println!("✓ Figment integration working correctly");
}

#[test]
fn test_env_var_override() {
    // Example 9: Test environment variable overrides
    use figment::{
        providers::{Format, Serialized, Toml},
        Figment,
    };

    // Set test env vars
    std::env::set_var("ZAINO_TEST_NETWORK", "mainnet");
    std::env::set_var("ZAINO_TEST_BACKEND", "state");

    let figment = Figment::new()
        .merge(Serialized::defaults(IndexerConfig::default()))
        .merge(Toml::file("tests/data/minimal.toml")) // This has testnet/fetch
        .merge(figment::providers::Env::prefixed("ZAINO_TEST_"));

    let config: IndexerConfig = figment
        .extract()
        .unwrap_or_else(|e| panic!("Figment failed with env override: {}", e));

    // Environment should override TOML
    assert_eq!(
        config.network,
        Network::Mainnet,
        "Env var should override TOML network"
    );
    assert_eq!(
        config.backend,
        BackendType::State,
        "Env var should override TOML backend"
    );

    // Clean up
    std::env::remove_var("ZAINO_TEST_NETWORK");
    std::env::remove_var("ZAINO_TEST_BACKEND");

    println!("✓ Environment variable override working correctly");
}

#[test]
fn test_invalid_toml_handling() {
    // Example 10: Test error handling for invalid TOML
    let invalid_configs = [
        // Invalid network
        ("backend = \"fetch\"\nnetwork = \"invalid_network\"", "Invalid network"),
        // Invalid backend  
        ("backend = \"invalid_backend\"\nnetwork = \"testnet\"", "Invalid backend"),
        // Invalid socket address
        ("backend = \"fetch\"\nnetwork = \"testnet\"\n[server]\njson_rpc_listen_address = \"invalid:address\"", "Invalid address"),
        // Missing required nested field
        ("backend = \"fetch\"\nnetwork = \"testnet\"\n[server.cookie]\n# missing enabled/disabled", "Missing cookie config"),
    ];

    for (invalid_toml, description) in invalid_configs {
        let result: Result<IndexerConfig, _> = toml::from_str(invalid_toml);

        assert!(result.is_err(), "Expected error for: {}", description);
        println!("✓ Correctly rejected invalid config: {}", description);
    }
}

#[test]
fn test_partial_config_with_defaults() {
    // Example 11: Test partial configs get proper defaults
    let partial_toml = r#"
network = "regtest"
backend = "fetch"

[server]
enable_json_server = true

[debug]  
no_sync = true
"#;

    let config: IndexerConfig = toml::from_str(partial_toml).unwrap();

    // Specified values
    assert_eq!(config.network, Network::Regtest);
    assert_eq!(config.backend, BackendType::Fetch);
    assert!(config.server.enable_json_server);
    assert!(config.debug.no_sync);

    // Should get defaults for unspecified values
    assert_eq!(
        config.server.grpc_listen_address,
        "127.0.0.1:8137".parse().unwrap()
    );
    assert_eq!(config.service.timeout, 30); // ServiceConfig default
    assert_eq!(config.validator.auth, AuthMethod::default()); // ValidatorConfig default
    assert!(!config.debug.no_db); // DebugConfig default

    println!("✓ Partial config with defaults working correctly");
}
