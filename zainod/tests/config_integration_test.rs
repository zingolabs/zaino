//! Integration test demonstrating programmatic configuration construction.
//!
//! This test shows how external consumers can easily build IndexerConfig 
//! for integration tests and different deployment scenarios.

use std::path::PathBuf;
use tempfile::TempDir;
use zainod::config::{DebugConfig, IndexerConfig, ServerConfig, StorageConfig};
use zaino_commons::config::{
    BackendType, CacheConfig, CookieAuth, DatabaseConfig, Network, ServiceConfig, ValidatorConfig,
};

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
            cookie: CookieAuth::Disabled, // no auth needed for tests
            grpc_listen_address: "127.0.0.1:0".parse().unwrap(),
            grpc_tls: false, // no TLS for local tests
            tls_cert_path: None,
            tls_key_path: None,
        },
        validator: ValidatorConfig {
            config: zebra_state::Config::default(),
            rpc_address: "127.0.0.1:18232".parse().unwrap(),
            indexer_rpc_address: "127.0.0.1:18230".parse().unwrap(),
            cookie: CookieAuth::Disabled, // test environment
            rpc_user: "test_user".to_string(),
            rpc_password: "test_password".to_string(),
        },
        service: ServiceConfig {
            timeout: 10, // shorter timeout for tests
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
            no_sync: true,  // disable sync for faster tests
            no_db: false,   // still want DB functionality
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
            config: zebra_state::Config {
                cache_dir: "/var/lib/zebra".into(),
                ephemeral: false,
                delete_old_database: false,
                debug_stop_at_height: None,
                debug_validity_check_interval: None,
            },
            rpc_address: "127.0.0.1:8232".parse().unwrap(),
            indexer_rpc_address: "127.0.0.1:8983".parse().unwrap(),
            cookie: CookieAuth::Enabled {
                path: "/var/lib/zebra/.cookie".into(),
            },
            rpc_user: "production_user".to_string(),
            rpc_password: "secure_password".to_string(),
        },
        service: ServiceConfig {
            timeout: 60, // longer timeout for production
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
        let zebra_network = config.network.to_zebra_network();
        
        // Can be serialized to string names
        let serialized = serde_json::to_string(&config.network).unwrap();
        assert!(serialized.contains(expected_name));
        
        // No string parsing errors possible
        assert_eq!(config.network, network);
    }
}