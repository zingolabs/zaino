#![allow(clippy::bool_assert_comparison)]

use figment::Jail;
use std::path::PathBuf;
use zaino_common::{DatabaseSize, Network};

// Use the explicit library name `zainodlib` as defined in Cargo.toml [lib] name.
use zainodlib::config::{load_config, ZainodConfig};
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
            storage.database.path = "{zaino_db_dir_name}"
            zebra_db_path = "{zebra_db_dir_name}"
            db_size = 100
            network = "Mainnet"
            no_db = false
            slow_sync = false

            [validator_settings]
            validator_jsonrpc_listen_address = "192.168.1.10:18232"
            validator_cookie_path = "{validator_cookie_file_name}"
            validator_user = "user"
            validator_password = "password"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8000"
            cookie_dir = "{zaino_cookie_dir_name}"

            [grpc_settings]
            listen_address = "0.0.0.0:9000"

            [grpc_settings.tls]
            cert_path = "{cert_file_name}"
            key_path = "{key_file_name}"
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
        assert!(finalized_config.json_server_settings.is_some());
        assert_eq!(
            finalized_config
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .json_rpc_listen_address,
            "127.0.0.1:8000".parse().unwrap()
        );
        assert_eq!(
            finalized_config
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .cookie_dir,
            Some(PathBuf::from(zaino_cookie_dir_name))
        );
        assert_eq!(
            finalized_config
                .clone()
                .grpc_settings
                .tls
                .expect("tls to be Some in finalized conifg")
                .cert_path,
            PathBuf::from(cert_file_name)
        );
        assert_eq!(
            finalized_config
                .clone()
                .grpc_settings
                .tls
                .expect("tls to be Some in finalized_conifg")
                .key_path,
            PathBuf::from(key_file_name)
        );
        assert_eq!(
            finalized_config.validator_settings.validator_cookie_path,
            Some(PathBuf::from(validator_cookie_file_name))
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
            finalized_config.grpc_settings.listen_address,
            "0.0.0.0:9000".parse().unwrap()
        );
        assert!(finalized_config.grpc_settings.tls.is_some());
        assert_eq!(
            finalized_config.validator_settings.validator_user,
            Some("user".to_string())
        );
        assert_eq!(
            finalized_config.validator_settings.validator_password,
            Some("password".to_string())
        );
        assert_eq!(finalized_config.storage.cache.capacity, 10000);
        assert_eq!(finalized_config.storage.cache.shard_count(), 16);
        assert_eq!(
            finalized_config.storage.database.size.to_byte_count(),
            128 * 1024 * 1024 * 1024
        );
        assert!(match finalized_config.storage.database.size {
            DatabaseSize::Gb(0) => false,
            DatabaseSize::Gb(_) => true,
        });

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
        let default_values = ZainodConfig::default();

        assert_eq!(config.backend, ZainoBackendType::State);
        assert_eq!(
            config.json_server_settings.is_some(),
            default_values.json_server_settings.is_some()
        );
        assert_eq!(
            config.validator_settings.validator_user,
            default_values.validator_settings.validator_user
        );
        assert_eq!(
            config.validator_settings.validator_password,
            default_values.validator_settings.validator_password
        );
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
        assert_eq!(
            config.storage.database.size.to_byte_count(),
            default_values.storage.database.size.to_byte_count()
        );
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

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = ""

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
        )?;

        let config1 = load_config(&s1_path).expect("Config S1 failed");
        assert!(config1.json_server_settings.is_some());
        assert!(config1
            .json_server_settings
            .as_ref()
            .expect("json settings is Some")
            .cookie_dir
            .is_some());

        // Scenario 2: auth enabled, cookie_dir specified
        let s2_path = jail.directory().join("s2.toml");
        jail.create_file(
            &s2_path,
            r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = "/my/cookie/path"

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
        )?;
        let config2 = load_config(&s2_path).expect("Config S2 failed");
        assert!(config2.json_server_settings.is_some());
        assert_eq!(
            config2
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .cookie_dir,
            Some(PathBuf::from("/my/cookie/path"))
        );
        let s3_path = jail.directory().join("s3.toml");
        jail.create_file(
            &s3_path,
            r#"
            backend = "fetch"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"

            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"
        "#,
        )?;
        let config3 = load_config(&s3_path).expect("Config S3 failed");
        assert!(config3
            .json_server_settings
            .expect("json server settings to unwrap in config S3")
            .cookie_dir
            .is_none());
        Ok(())
    });
}

#[test]
fn test_string_none_as_path_for_cookie_dir() {
    Jail::expect_with(|jail| {
        let toml_auth_enabled_path = jail.directory().join("auth_enabled.toml");
        // cookie auth on but no dir assigned
        jail.create_file(
            &toml_auth_enabled_path,
            r#"
            backend = "fetch"
            grpc_listen_address = "127.0.0.1:8137"
            validator_listen_address = "127.0.0.1:18232"
            zaino_db_path = "/zaino/db"
            zebra_db_path = "/zebra/db"
            network = "Testnet"

            [json_server_settings]
            json_rpc_listen_address = "127.0.0.1:8237"
            cookie_dir = ""
        "#,
        )?;
        let config_auth_enabled =
            load_config(&toml_auth_enabled_path).expect("Auth enabled failed");
        assert!(config_auth_enabled.json_server_settings.is_some());
        assert!(config_auth_enabled
            .json_server_settings
            .as_ref()
            .expect("json settings to be Some")
            .cookie_dir
            .is_some());

        // omitting cookie_dir will set it to None
        let toml_auth_disabled_path = jail.directory().join("auth_disabled.toml");
        jail.create_file(
            &toml_auth_disabled_path,
            r#"
            backend = "fetch"

            [json_server_settings]
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
        assert!(config_auth_disabled.json_server_settings.is_some());
        assert_eq!(
            config_auth_disabled
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .cookie_dir,
            None
        );
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
        let default_config = ZainodConfig::default();
        // Compare relevant fields that should come from default
        assert_eq!(config.network, default_config.network);
        assert_eq!(config.backend, default_config.backend);
        assert_eq!(
            config.json_server_settings.is_some(),
            default_config.json_server_settings.is_some()
        );
        assert_eq!(
            config.validator_settings.validator_user,
            default_config.validator_settings.validator_user
        );
        assert_eq!(
            config.validator_settings.validator_password,
            default_config.validator_settings.validator_password
        );
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
        assert_eq!(
            config.storage.database.size.to_byte_count(),
            default_config.storage.database.size.to_byte_count()
        );
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
            r#"
            [json_server_settings]
            json_rpc_listen_address = "not-a-valid-address"
            cookie_dir = ""
            "#,
        )?;
        let result = load_config(&invalid_toml_path);
        assert!(result.is_err());
        if let Err(IndexerError::ConfigError(msg)) = result {
            assert!(msg.contains("invalid socket address syntax"));
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
        let defaults = ZainodConfig::default();

        assert_eq!(config.backend, ZainoBackendType::Fetch);
        assert_eq!(
            config.validator_settings.validator_user,
            defaults.validator_settings.validator_user
        );

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
        "#,
        )?;
        jail.set_env("ZAINO_NETWORK", "Mainnet");
        jail.set_env(
            "ZAINO_JSON_SERVER_SETTINGS-JSON_RPC_LISTEN_ADDRESS",
            "127.0.0.1:0",
        );
        jail.set_env("ZAINO_JSON_SERVER_SETTINGS-COOKIE_DIR", "/env/cookie/path");
        jail.set_env("ZAINO_STORAGE.CACHE.CAPACITY", "12345");

        let temp_toml_path = jail.directory().join("test_config.toml");
        let config = load_config(&temp_toml_path).expect("load_config should succeed");

        assert_eq!(config.network, Network::Mainnet);
        assert_eq!(config.storage.cache.capacity, 12345);
        assert!(config.json_server_settings.is_some());
        assert_eq!(
            config
                .json_server_settings
                .as_ref()
                .expect("json settings to be Some")
                .cookie_dir,
            Some(PathBuf::from("/env/cookie/path"))
        );
        assert!(config.grpc_settings.tls.is_none());
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

            [json_server_settings]
            json_rpc_listen_address = ""
            cookie_dir = ""
        "#,
        )?;
        let temp_toml_path = jail.directory().join("test_config.toml");
        // a json_server_setting without a listening address is forbidden
        assert!(load_config(&temp_toml_path).is_err());
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
        let defaults = ZainodConfig::default();
        assert_eq!(config.network, defaults.network);
        assert_eq!(
            config.json_server_settings.is_some(),
            defaults.json_server_settings.is_some()
        );
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
