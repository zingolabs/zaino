#![allow(clippy::bool_assert_comparison)]

use figment::Jail;
use std::path::PathBuf;
use zaino_common::network::ActivationHeights;
use zaino_common::Network;

// Use the explicit library name `zainodlib` as defined in Cargo.toml [lib] name.
use zainodlib::config::{load_config, IndexerConfig};
use zainodlib::error::IndexerError;
// If BackendType is used directly in assertions beyond what IndexerConfig holds:
use zaino_state::BackendType as ZainoBackendType;

#[test]
// Validates loading a valid configuration via `load_config`,
// ensuring fields are parsed and `check_config` passes with mocked prerequisite files.
fn test_deserialize_full_valid_config() {
    Jail::expect_with(|jail| {
        // Define RELATIVE paths/filenames for use within the jail
        let cert_file_name = "test_cert.pem";
        let key_file_name = "test_key.pem";
        let validator_cookie_file_name = "validator.cookie";
        let zaino_cookie_dir_name = "zaino_cookies_dir";
        let zaino_db_dir_name = "zaino_db_dir";
        let zebra_db_dir_name = "zebra_db_dir";

        // Create the directories within the jail FIRST
        jail.create_dir(zaino_cookie_dir_name)?;
        jail.create_dir(zaino_db_dir_name)?;
        jail.create_dir(zebra_db_dir_name)?;

        // Use relative paths in the TOML string
        let toml_str = format!(
            r#"
            backend = "fetch"
            enable_json_server = true
            json_rpc_listen_address = "127.0.0.1:8000"
            enable_cookie_auth = true
            cookie_dir = "{zaino_cookie_dir_name}"
            grpc_listen_address = "0.0.0.0:9000"
            grpc_tls = true
            tls_cert_path = "{cert_file_name}"
            tls_key_path = "{key_file_name}"
            validator_listen_address = "192.168.1.10:18232"
            validator_cookie_auth = true
            validator_cookie_path = "{validator_cookie_file_name}"
            validator_user = "user"
            validator_password = "password"
            storage.database.path = "{zaino_db_dir_name}"
            zebra_db_path = "{zebra_db_dir_name}"
            db_size = 100
            network = "Mainnet"
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

        assert_eq!(finalized_config.backend, ZainoBackendType::Fetch);
        assert!(finalized_config.enable_json_server);
        assert_eq!(
            finalized_config.json_rpc_listen_address,
            "127.0.0.1:8000".parse().unwrap()
        );
        assert!(finalized_config.enable_cookie_auth);
        assert_eq!(
            finalized_config.cookie_dir,
            Some(PathBuf::from(zaino_cookie_dir_name))
        );
        assert_eq!(
            finalized_config.tls_cert_path,
            Some(cert_file_name.to_string())
        );
        assert_eq!(
            finalized_config.tls_key_path,
            Some(key_file_name.to_string())
        );
        assert_eq!(
            finalized_config.validator_cookie_path,
            Some(validator_cookie_file_name.to_string())
        );
        assert_eq!(
            finalized_config.storage.database.path,
            PathBuf::from(zaino_db_dir_name)
        );
        assert_eq!(
            finalized_config.zebra_db_path,
            PathBuf::from(zebra_db_dir_name)
        );
        assert_eq!(finalized_config.network, Network::Mainnet);
        assert_eq!(
            finalized_config.grpc_listen_address,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert!(finalized_config.grpc_tls);
        assert_eq!(finalized_config.validator_user, Some("user".to_string()));
        assert_eq!(
            finalized_config.validator_password,
            Some("password".to_string())
        );
        assert_eq!(finalized_config.storage.cache.capacity, 10000);
        assert_eq!(finalized_config.storage.cache.shard_count(), 16);
        assert_eq!(
            finalized_config.storage.database.size.to_byte_count(),
            128 * 1024 * 1024 * 1024
        );
        assert!(!finalized_config.no_sync);
        assert!(!finalized_config.no_db);
        assert!(!finalized_config.slow_sync);

        Ok(())
    });
}

#[test]
// Verifies that when optional fields are omitted from TOML, `load_config` ensures they correctly adopt default values.
fn test_deserialize_optional_fields_missing() {
    Jail::expect_with(|jail| {
        let toml_str = r#"
            backend = "state"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/opt/zaino/data"
            zebra_db_path = "/opt/zebra/data"
            network = "Testnet"
        "#;
        let temp_toml_path = jail.directory().join("optional_missing.toml");
        jail.create_file(&temp_toml_path, toml_str)?;

        let config = load_config(&temp_toml_path).expect("load_config failed");
        let default_values = IndexerConfig::default();

        assert_eq!(config.backend, ZainoBackendType::State);
        assert_eq!(config.enable_json_server, default_values.enable_json_server);
        assert_eq!(config.validator_user, default_values.validator_user);
        assert_eq!(config.validator_password, default_values.validator_password);
        assert_eq!(
            config.storage.cache.capacity,
            default_values.storage.cache.capacity
        );
        assert_eq!(
            config.storage.cache.shard_count(),
            default_values.storage.cache.shard_count(),
        );
        assert_eq!(
            config.storage.database.size,
            default_values.storage.database.size
        );
        assert_eq!(config.no_sync, default_values.no_sync);
        assert_eq!(config.no_db, default_values.no_db);
        assert_eq!(config.slow_sync, default_values.slow_sync);
        Ok(())
    });
}

#[test]
// Tests the logic (via `load_config` and its internal call to `finalize_config_logic`)
// for setting `cookie_dir` based on `enable_cookie_auth`.
fn test_cookie_dir_logic() {
    Jail::expect_with(|jail| {
        // Scenario 1: auth enabled, cookie_dir missing (should use default ephemeral path)
        let s1_path = jail.directory().join("s1.toml");
        jail.create_file(
            &s1_path,
            r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = true
        "#,
        )?;

        let config1 = load_config(&s1_path).expect("Config S1 failed");
        assert!(config1.cookie_dir.is_some());

        // Scenario 2: auth enabled, cookie_dir specified
        let s2_path = jail.directory().join("s2.toml");
        jail.create_file(
            &s2_path,
            r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = true
            cookie_dir = "/my/cookie/path"
        "#,
        )?;
        let config2 = load_config(&s2_path).expect("Config S2 failed");
        assert_eq!(config2.cookie_dir, Some(PathBuf::from("/my/cookie/path")));

        // Scenario 3: auth disabled, cookie_dir specified (should be None after finalize)
        let s3_path = jail.directory().join("s3.toml");
        jail.create_file(
            &s3_path,
            r#"
            backend = "fetch"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
            enable_cookie_auth = false
            cookie_dir = "/my/ignored/path"
        "#,
        )?;
        let config3 = load_config(&s3_path).expect("Config S3 failed");
        assert_eq!(config3.cookie_dir, None);
        Ok(())
    });
}

#[test]
// Tests how `load_config` handles literal string "None" for `cookie_dir`
// with varying `enable_cookie_auth` states.
fn test_string_none_as_path_for_cookie_dir() {
    Jail::expect_with(|jail| {
        let toml_auth_enabled_path = jail.directory().join("auth_enabled.toml");
        jail.create_file(
            &toml_auth_enabled_path,
            r#"
            backend = "fetch"
            enable_cookie_auth = true
            cookie_dir = "None"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
        )?;
        let config_auth_enabled =
            load_config(&toml_auth_enabled_path).expect("Auth enabled failed");
        assert_eq!(config_auth_enabled.cookie_dir, Some(PathBuf::from("None")));

        let toml_auth_disabled_path = jail.directory().join("auth_disabled.toml");
        jail.create_file(
            &toml_auth_disabled_path,
            r#"
            backend = "fetch"
            enable_cookie_auth = false
            cookie_dir = "None"
            json_rpc_listen_address = "127.0.0.1:8237"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
        )?;
        let config_auth_disabled =
            load_config(&toml_auth_disabled_path).expect("Auth disabled failed");
        assert_eq!(config_auth_disabled.cookie_dir, None);
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
        assert_eq!(config.enable_json_server, default_config.enable_json_server);
        assert_eq!(config.validator_user, default_config.validator_user);
        assert_eq!(config.validator_password, default_config.validator_password);
        assert_eq!(
            config.storage.cache.capacity,
            default_config.storage.cache.capacity
        );
        assert_eq!(
            config.storage.cache.shard_count(),
            default_config.storage.cache.shard_count()
        );
        assert_eq!(
            config.storage.database.size,
            default_config.storage.database.size
        );
        assert_eq!(config.no_sync, default_config.no_sync);
        assert_eq!(config.no_db, default_config.no_db);
        assert_eq!(config.slow_sync, default_config.slow_sync);
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
            assert!(msg.contains("Invalid backend type"));
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
            r#"json_rpc_listen_address = "not-a-valid-address""#,
        )?;
        let result = load_config(&invalid_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("Invalid socket address string"));
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

        assert_eq!(config.backend, ZainoBackendType::Fetch);
        assert_eq!(config.validator_user, defaults.validator_user);

        Ok(())
    });
}

// Figment-specific tests below are generally self-descriptive by name
#[test]
fn test_figment_env_override_toml_and_defaults() {
    Jail::expect_with(|jail| {
        jail.create_file(
            "test_config.toml",
            r#"
            network = "Testnet"
            enable_json_server = false
        "#,
        )?;
        jail.set_env("ZAINO_NETWORK", "Mainnet");
        jail.set_env("ZAINO_ENABLE_JSON_SERVER", "true");
        jail.set_env("ZAINO_STORAGE.CACHE.CAPACITY", "12345");
        jail.set_env("ZAINO_ENABLE_COOKIE_AUTH", "true");
        jail.set_env("ZAINO_COOKIE_DIR", "/env/cookie/path");

        let temp_toml_path = jail.directory().join("test_config.toml");
        let config = load_config(&temp_toml_path).expect("load_config should succeed");

        assert_eq!(config.network, Network::Mainnet);
        assert!(config.enable_json_server);
        assert_eq!(config.storage.cache.capacity, 12345);
        assert_eq!(config.cookie_dir, Some(PathBuf::from("/env/cookie/path")));
        assert_eq!(!config.grpc_tls, true);
        Ok(())
    });
}

#[test]
fn test_figment_toml_overrides_defaults() {
    Jail::expect_with(|jail| {
        jail.create_file(
            "test_config.toml",
            r#"
            network = "Regtest"
            enable_json_server = true
        "#,
        )?;
        let temp_toml_path = jail.directory().join("test_config.toml");
        let config = load_config(&temp_toml_path).expect("load_config should succeed");
        assert_eq!(
            config.network,
            Network::Regtest(ActivationHeights::default())
        );
        assert!(config.enable_json_server);
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
        assert_eq!(config.enable_json_server, defaults.enable_json_server);
        assert_eq!(
            config.storage.cache.capacity,
            defaults.storage.cache.capacity
        );
        Ok(())
    });
}

#[test]
fn test_figment_invalid_env_var_type() {
    Jail::expect_with(|jail| {
        jail.create_file("test_config.toml", "")?;
        jail.set_env("ZAINO_STORAGE.CACHE.CAPACITY", "not_a_number");
        let temp_toml_path = jail.directory().join("test_config.toml");
        let result = load_config(&temp_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.to_lowercase().contains("storage.cache.capacity") && msg.contains("invalid type"),
                    "Error message should mention 'map_capacity' (case-insensitive) and 'invalid type'. Got: {msg}");
        } else {
            panic!("Expected ConfigError, got {result:?}");
        }
        Ok(())
    });
}
