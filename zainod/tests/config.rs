#![allow(clippy::bool_assert_comparison)]

use figment::Jail;
use std::path::PathBuf;

// Use the explicit library name `zainodlib` as defined in Cargo.toml [lib] name.
use zainodlib::config::{load_config, IndexerConfig};
use zainodlib::error::IndexerError;
use zaino_commons::config::{BackendType, CookieAuth, Network};

#[test]
// Validates loading a valid configuration via `load_config`,
// ensuring fields are parsed and `check_config` passes with mocked prerequisite files.
fn test_deserialize_full_valid_config() {
    Jail::expect_with(|jail| {
        // Define RELATIVE paths/filenames for use within the jail
        let cert_file_name = "test_cert.pem";
        let key_file_name = "test_key.pem";
        let validator_cookie_file_name = "validator.cookie";
        let zaino_db_dir_name = "zaino_db_dir";
        let zebra_db_dir_name = "zebra_db_dir";

        // Create the directories within the jail FIRST
        jail.create_dir(zaino_db_dir_name)?;
        jail.create_dir(zebra_db_dir_name)?;

        // Use the new nested TOML structure
        let toml_str = format!(
            r#"
            backend = "fetch"
            network = "mainnet"
            
            [server]
            enable_json_server = true
            json_rpc_listen_address = "127.0.0.1:8000"
            grpc_listen_address = "0.0.0.0:9000"
            grpc_tls = true
            tls_cert_path = "{cert_file_name}"
            tls_key_path = "{key_file_name}"
            
            [server.cookie]
            enabled = {{ path = "{validator_cookie_file_name}" }}
            
            [validator]
            rpc_address = "192.168.1.10:18232"
            indexer_rpc_address = "192.168.1.10:18230"
            rpc_user = "user"
            rpc_password = "password"
            
            [validator.cookie]
            enabled = {{ path = "{validator_cookie_file_name}" }}
            
            [validator.config]
            cache_dir = "{zebra_db_dir_name}"
            ephemeral = false
            delete_old_database = false
            
            [service]
            timeout = 60
            channel_size = 128
            
            [storage]
            [storage.cache]
            capacity = 10000
            shard_amount = 16
            
            [storage.zaino_database]
            path = "{zaino_db_dir_name}"
            size = 100
            
            [storage.zebra_database]
            path = "{zebra_db_dir_name}"
            
            [debug]
            no_sync = false
            no_db = false
            slow_sync = false
        "#
        );

        let temp_toml_path = jail.directory().join("full_config.toml");
        jail.create_file(&temp_toml_path, &toml_str)?;

        // Create the actual mock files within the jail using the relative names
        jail.create_file(cert_file_name, "mock cert content")?;
        jail.create_file(key_file_name, "mock key content")?;
        jail.create_file(validator_cookie_file_name, "mock validator cookie content")?;

        let config_result = load_config(&temp_toml_path);
        assert!(
            config_result.is_ok(),
            "load_config failed: {:?}",
            config_result.err()
        );
        let finalized_config = config_result.unwrap();

        // Test the new nested structure
        assert_eq!(finalized_config.backend, BackendType::Fetch);
        assert_eq!(finalized_config.network, Network::Mainnet);
        assert!(finalized_config.server.enable_json_server);
        assert_eq!(
            finalized_config.server.json_rpc_listen_address,
            "127.0.0.1:8000".parse().unwrap()
        );
        assert!(matches!(finalized_config.server.cookie, CookieAuth::Enabled { .. }));
        assert_eq!(
            finalized_config.server.tls_cert_path,
            Some(cert_file_name.to_string())
        );
        assert_eq!(
            finalized_config.server.tls_key_path,
            Some(key_file_name.to_string())
        );
        assert!(matches!(finalized_config.validator.cookie, CookieAuth::Enabled { .. }));
        assert_eq!(
            finalized_config.storage.zaino_database.path,
            PathBuf::from(zaino_db_dir_name)
        );
        assert_eq!(
            finalized_config.storage.zebra_database.path,
            PathBuf::from(zebra_db_dir_name)
        );
        assert_eq!(
            finalized_config.server.grpc_listen_address,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert!(finalized_config.server.grpc_tls);
        assert_eq!(finalized_config.validator.rpc_user, "user".to_string());
        assert_eq!(finalized_config.validator.rpc_password, "password".to_string());
        assert_eq!(finalized_config.storage.cache.capacity, Some(10000));
        assert_eq!(finalized_config.storage.cache.shard_amount, Some(16));
        assert_eq!(finalized_config.storage.zaino_database.size, Some(100));
        assert!(!finalized_config.debug.no_sync);
        assert!(!finalized_config.debug.no_db);
        assert!(!finalized_config.debug.slow_sync);

        Ok(())
    });
}

#[test]
// Verifies that when optional fields are omitted from TOML, `load_config` ensures they correctly adopt default values.
fn test_deserialize_optional_fields_missing() {
    Jail::expect_with(|jail| {
        let toml_str = r#"
            backend = "state"
            network = "testnet"
            
            [server]
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            
            [validator]
            rpc_address = "127.0.0.1:18232"
            indexer_rpc_address = "127.0.0.1:18230"
            
            [storage.zaino_database]
            path = "/opt/zaino/data"
            
            [storage.zebra_database]
            path = "/opt/zebra/data"
        "#;
        let temp_toml_path = jail.directory().join("optional_missing.toml");
        jail.create_file(&temp_toml_path, toml_str)?;

        let config = load_config(&temp_toml_path).expect("load_config failed");
        let default_values = IndexerConfig::default();

        assert_eq!(config.backend, BackendType::State);
        assert_eq!(config.network, Network::Testnet);
        assert_eq!(config.server.enable_json_server, default_values.server.enable_json_server);
        assert_eq!(config.validator.rpc_user, default_values.validator.rpc_user);
        assert_eq!(config.validator.rpc_password, default_values.validator.rpc_password);
        assert_eq!(config.storage.cache.capacity, default_values.storage.cache.capacity);
        assert_eq!(config.storage.cache.shard_amount, default_values.storage.cache.shard_amount);
        assert_eq!(config.storage.zaino_database.size, default_values.storage.zaino_database.size);
        assert_eq!(config.debug.no_sync, default_values.debug.no_sync);
        assert_eq!(config.debug.no_db, default_values.debug.no_db);
        assert_eq!(config.debug.slow_sync, default_values.debug.slow_sync);
        Ok(())
    });
}

#[test]
// Tests the logic for cookie authentication settings.
fn test_cookie_auth_logic() {
    Jail::expect_with(|jail| {
        // Scenario 1: server auth enabled
        let s1_path = jail.directory().join("s1.toml");
        jail.create_file(
            &s1_path,
            r#"
            backend = "fetch"
            network = "testnet"
            
            [server]
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            
            [server.cookie]
            enabled = {{ path = "/my/cookie/path" }}
            
            [validator]
            rpc_address = "127.0.0.1:18232"
            indexer_rpc_address = "127.0.0.1:18230"
            
            [storage.zaino_database]
            path = "/zaino/db"
            
            [storage.zebra_database]
            path = "/zebra/db"
        "#,
        )?;

        let config1 = load_config(&s1_path).expect("Config S1 failed");
        assert!(matches!(config1.server.cookie, CookieAuth::Enabled { .. }));

        // Scenario 2: auth disabled
        let s2_path = jail.directory().join("s2.toml");
        jail.create_file(
            &s2_path,
            r#"
            backend = "fetch"
            network = "testnet"
            
            [server]
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            cookie = "disabled"
            
            [validator]
            rpc_address = "127.0.0.1:18232"
            indexer_rpc_address = "127.0.0.1:18230"
            
            [storage.zaino_database]
            path = "/zaino/db"
            
            [storage.zebra_database]
            path = "/zebra/db"
        "#,
        )?;
        let config2 = load_config(&s2_path).expect("Config S2 failed");
        assert!(matches!(config2.server.cookie, CookieAuth::Disabled));
        Ok(())
    });
}

#[test]
// Checks that `load_config` with an empty TOML string results in the default `IndexerConfig` values.
fn test_deserialize_empty_string_yields_default() {
    Jail::expect_with(|jail| {
        let empty_toml_path = jail.directory().join("empty.toml");
        jail.create_file(&empty_toml_path, "")?;
        let config = load_config(&empty_toml_path).expect("Empty TOML load failed");
        let default_config = IndexerConfig::default();
        // Compare relevant fields that should come from default
        assert_eq!(config.network, default_config.network);
        assert_eq!(config.backend, default_config.backend);
        assert_eq!(config.server.enable_json_server, default_config.server.enable_json_server);
        assert_eq!(config.validator.rpc_user, default_config.validator.rpc_user);
        assert_eq!(config.validator.rpc_password, default_config.validator.rpc_password);
        assert_eq!(config.storage.cache.capacity, default_config.storage.cache.capacity);
        assert_eq!(config.storage.cache.shard_amount, default_config.storage.cache.shard_amount);
        assert_eq!(config.storage.zaino_database.size, default_config.storage.zaino_database.size);
        assert_eq!(config.debug.no_sync, default_config.debug.no_sync);
        assert_eq!(config.debug.no_db, default_config.debug.no_db);
        assert_eq!(config.debug.slow_sync, default_config.debug.slow_sync);
        Ok(())
    });
}

#[test]
// Ensures `load_config` returns an error for an invalid `backend` type string in TOML.
fn test_deserialize_invalid_backend_type() {
    Jail::expect_with(|jail| {
        let invalid_toml_path = jail.directory().join("invalid_backend.toml");
        jail.create_file(&invalid_toml_path, r#"backend = "invalid_type""#)?;
        let result = load_config(&invalid_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("invalid type") || msg.contains("unknown variant"));
        }
        Ok(())
    });
}

#[test]
// Ensures `load_config` returns an error for an invalid socket address string in TOML.
fn test_deserialize_invalid_socket_address() {
    Jail::expect_with(|jail| {
        let invalid_toml_path = jail.directory().join("invalid_socket.toml");
        jail.create_file(
            &invalid_toml_path,
            r#"
            [server]
            json_rpc_listen_address = "not-a-valid-address"
            "#,
        )?;
        let result = load_config(&invalid_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("Invalid socket address string") || msg.contains("invalid type"));
        }
        Ok(())
    });
}

#[test]
// Validates that the actual zindexer.toml file (with optional values commented out)
// is parsed correctly by `load_config`, applying defaults for missing optional fields.
fn test_parse_zindexer_toml_integration() {
    let zindexer_toml_content = include_str!("../zindexer.toml");

    Jail::expect_with(|jail| {
        let temp_toml_path = jail.directory().join("zindexer_test.toml");
        jail.create_file(&temp_toml_path, zindexer_toml_content)?;

        let config_result = load_config(&temp_toml_path);
        assert!(
            config_result.is_ok(),
            "load_config failed to parse zindexer.toml: {:?}",
            config_result.err()
        );
        let config = config_result.unwrap();
        let defaults = IndexerConfig::default();

        assert_eq!(config.backend, BackendType::Fetch);
        assert_eq!(config.validator.rpc_user, defaults.validator.rpc_user);

        Ok(())
    });
}

// Figment-specific tests below use the proper nested configuration structure
#[test]
fn test_figment_env_override_toml_and_defaults() {
    Jail::expect_with(|jail| {
        jail.create_file(
            "test_config.toml",
            r#"
            network = "testnet"
            
            [server]
            enable_json_server = false
        "#,
        )?;
        jail.set_env("ZAINO_NETWORK", "mainnet");
        jail.set_env("ZAINO_SERVER__ENABLE_JSON_SERVER", "true");
        jail.set_env("ZAINO_STORAGE__CACHE__CAPACITY", "12345");

        let temp_toml_path = jail.directory().join("test_config.toml");
        let config = load_config(&temp_toml_path).expect("load_config should succeed");

        assert_eq!(config.network, Network::Mainnet);
        assert!(config.server.enable_json_server);
        assert_eq!(config.storage.cache.capacity, Some(12345));
        assert!(!config.server.grpc_tls);
        Ok(())
    });
}

#[test]
fn test_figment_toml_overrides_defaults() {
    Jail::expect_with(|jail| {
        jail.create_file(
            "test_config.toml",
            r#"
            network = "regtest"
            
            [server]
            enable_json_server = true
        "#,
        )?;
        let temp_toml_path = jail.directory().join("test_config.toml");
        let config = load_config(&temp_toml_path).expect("load_config should succeed");
        assert_eq!(config.network, Network::Regtest);
        assert!(config.server.enable_json_server);
        Ok(())
    });
}

#[test]
fn test_figment_all_defaults() {
    Jail::expect_with(|jail| {
        jail.create_file("empty_config.toml", "")?;
        let temp_toml_path = jail.directory().join("empty_config.toml");
        let config =
            load_config(&temp_toml_path).expect("load_config should succeed with empty toml");
        let defaults = IndexerConfig::default();
        assert_eq!(config.network, defaults.network);
        assert_eq!(config.server.enable_json_server, defaults.server.enable_json_server);
        assert_eq!(config.storage.cache.capacity, defaults.storage.cache.capacity);
        Ok(())
    });
}

#[test]
fn test_figment_invalid_env_var_type() {
    Jail::expect_with(|jail| {
        jail.create_file("test_config.toml", "")?;
        jail.set_env("ZAINO_STORAGE__CACHE__CAPACITY", "not_a_number");
        let temp_toml_path = jail.directory().join("test_config.toml");
        let result = load_config(&temp_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.to_lowercase().contains("capacity") || msg.contains("invalid type"),
                    "Error message should mention 'capacity' or 'invalid type'. Got: {msg}");
        } else {
            panic!("Expected ConfigError, got {result:?}");
        }
        Ok(())
    });
}