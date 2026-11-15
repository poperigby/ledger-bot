use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use config::{Environment, File, FileFormat, Map};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Config {
    pub matrix: MatrixConfig,
    pub postgres: PostgresConfig,
    pub data_dir: String,
}

#[derive(Debug, Clone)]
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub device_id: Option<String>,
    pub access_token: Option<SecretSource>,
    pub password: Option<SecretSource>,
    pub store_passphrase: Option<SecretSource>,
    pub allowed_inviter_mxids: Vec<String>,
    pub allowed_room_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub database_url: Option<SecretSource>,
    pub max_connections: u32,
}

#[derive(Debug, Clone)]
pub enum SecretSource {
    File(PathBuf),
    Value(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    matrix: FileMatrixConfig,
    postgres: FilePostgresConfig,
    #[serde(default = "default_data_dir")]
    data_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMatrixConfig {
    homeserver_url: String,
    user_id: String,
    device_id: Option<String>,
    access_token: Option<SecretFile>,
    password: Option<SecretFile>,
    store_passphrase: Option<SecretFile>,
    #[serde(default)]
    allowed_inviter_mxids: Vec<String>,
    #[serde(default)]
    allowed_room_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePostgresConfig {
    database_url: Option<SecretFile>,
    #[serde(default = "default_max_connections")]
    max_connections: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    file: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EnvSecrets {
    matrix_access_token: Option<String>,
    matrix_password: Option<String>,
    matrix_store_passphrase: Option<String>,
    postgres_database_url: Option<String>,
}

impl Config {
    pub fn from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path.as_ref())?;
        Self::from_str_and_env(&text)
    }

    fn from_str_and_env(text: &str) -> anyhow::Result<Self> {
        let file_config = config::Config::builder()
            .add_source(File::from_str(text, FileFormat::Toml))
            .build()?
            .try_deserialize::<FileConfig>()?;
        let env_secrets = env_secrets()?;

        Ok(file_config.with_env_secrets(env_secrets))
    }
}

impl FileConfig {
    fn with_env_secrets(self, env: EnvSecrets) -> Config {
        Config {
            matrix: MatrixConfig {
                homeserver_url: self.matrix.homeserver_url,
                user_id: self.matrix.user_id,
                device_id: self.matrix.device_id,
                access_token: self
                    .matrix
                    .access_token
                    .map(Into::into)
                    .or_else(|| env.matrix_access_token.map(SecretSource::Value)),
                password: self
                    .matrix
                    .password
                    .map(Into::into)
                    .or_else(|| env.matrix_password.map(SecretSource::Value)),
                store_passphrase: self
                    .matrix
                    .store_passphrase
                    .map(Into::into)
                    .or_else(|| env.matrix_store_passphrase.map(SecretSource::Value)),
                allowed_inviter_mxids: self.matrix.allowed_inviter_mxids,
                allowed_room_ids: self.matrix.allowed_room_ids,
            },
            postgres: PostgresConfig {
                database_url: self
                    .postgres
                    .database_url
                    .map(Into::into)
                    .or_else(|| env.postgres_database_url.map(SecretSource::Value)),
                max_connections: self.postgres.max_connections,
            },
            data_dir: self.data_dir,
        }
    }
}

impl From<SecretFile> for SecretSource {
    fn from(value: SecretFile) -> Self {
        Self::File(value.file)
    }
}

impl SecretSource {
    pub fn resolve(&self, name: &str) -> anyhow::Result<String> {
        match self {
            Self::File(path) => {
                let value = fs::read_to_string(path)
                    .with_context(|| format!("failed to read {name} from {}", path.display()))?;
                Ok(trim_secret(&value))
            }
            Self::Value(value) => Ok(value.clone()),
        }
    }
}

pub fn resolve_optional_secret(
    source: Option<&SecretSource>,
    name: &str,
) -> anyhow::Result<Option<String>> {
    source.map(|source| source.resolve(name)).transpose()
}

pub fn resolve_required_secret(
    source: Option<&SecretSource>,
    name: &str,
) -> anyhow::Result<String> {
    resolve_optional_secret(source, name)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{name} must be set with a secret file or LEDGER__{}",
                env_key(name)
            )
        })
}

fn env_secrets() -> anyhow::Result<EnvSecrets> {
    env_secrets_from(None)
}

fn env_secrets_from(source: Option<Map<String, String>>) -> anyhow::Result<EnvSecrets> {
    Ok(config::Config::builder()
        .add_source(
            Environment::with_prefix("LEDGER")
                .prefix_separator("__")
                .ignore_empty(true)
                .source(source),
        )
        .build()?
        .try_deserialize()?)
}

fn env_key(name: &str) -> String {
    name.replace('.', "_").to_ascii_uppercase()
}

fn trim_secret(value: &str) -> String {
    value.trim_end_matches(['\r', '\n']).to_owned()
}

fn default_data_dir() -> String {
    "/var/lib/ledger-bot".to_owned()
}

fn default_max_connections() -> u32 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config = Config::from_str_and_env(
            r#"
data_dir = "/tmp/ledger-bot"

[matrix]
homeserver_url = "https://matrix.example"
user_id = "@ledger:example"
allowed_room_ids = ["!room:example"]

[matrix.access_token]
file = "/run/secrets/ledger-bot/matrix-access-token"

[postgres]
max_connections = 10
"#,
        )
        .unwrap();

        assert_eq!(config.data_dir, "/tmp/ledger-bot");
        assert_eq!(config.postgres.max_connections, 10);
        assert_eq!(config.matrix.allowed_room_ids, ["!room:example"]);
        assert!(config.postgres.database_url.is_none());
    }

    #[test]
    fn rejects_inline_secrets() {
        let error = Config::from_str_and_env(
            r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@ledger:example"
access_token = "secret"

[postgres]
database_url = "postgres://ledger:ledger@localhost/ledger"
"#,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("invalid type")
                || error.to_string().contains("invalid type: string"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn resolves_secret_from_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("secret");
        fs::write(&path, "secret-value\n").unwrap();

        let source = SecretSource::File(path);

        assert_eq!(source.resolve("test.secret").unwrap(), "secret-value");
    }

    #[test]
    fn rejects_configurable_env_secret_names() {
        let error = Config::from_str_and_env(
            r#"
[matrix]
homeserver_url = "https://matrix.example"
user_id = "@ledger:example"

[matrix.password]
env = "MATRIX_PASSWORD"

[postgres]
"#,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("missing field `file`")
                || error.to_string().contains("unknown field `env`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn reads_standardized_env_secret_names() {
        let secrets = env_secrets_from(Some(Map::from_iter([
            (
                "LEDGER__MATRIX_ACCESS_TOKEN".to_owned(),
                "access-token".to_owned(),
            ),
            ("LEDGER__MATRIX_PASSWORD".to_owned(), "password".to_owned()),
            (
                "LEDGER__MATRIX_STORE_PASSPHRASE".to_owned(),
                "store-passphrase".to_owned(),
            ),
            (
                "LEDGER__POSTGRES_DATABASE_URL".to_owned(),
                "postgres://ledger@example/ledger".to_owned(),
            ),
        ])))
        .unwrap();

        assert_eq!(secrets.matrix_access_token.as_deref(), Some("access-token"));
        assert_eq!(secrets.matrix_password.as_deref(), Some("password"));
        assert_eq!(
            secrets.matrix_store_passphrase.as_deref(),
            Some("store-passphrase")
        );
        assert_eq!(
            secrets.postgres_database_url.as_deref(),
            Some("postgres://ledger@example/ledger")
        );
    }
}
