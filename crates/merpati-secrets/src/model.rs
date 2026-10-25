//! Provider configurations and linked vault secrets.
//!
//! Everything here is non-secret and is stored in the workspace's git-ignored
//! local folder (`.merpati/external-secrets.json`). Credentials (tokens,
//! client secrets, access keys) live in the workspace [`SecretStore`] under
//! [`credential_key`]`(provider_id, field)`.
//!
//! [`SecretStore`]: merpati_core::secrets::SecretStore

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::SecretError;

/// File name of the configuration inside the workspace local folder.
pub const CONFIG_FILE: &str = "external-secrets.json";

/// Prefix of credential keys in the secret store.
pub const CREDENTIAL_KEY_PREFIX: &str = "extsecrets:";

/// Default cache lifetime of a linked secret.
pub const DEFAULT_TTL_SECS: u64 = 300;

/// Secret store key of one credential field of a provider:
/// `extsecrets:<provider id>:<field>`.
pub fn credential_key(provider_id: &str, field: &str) -> String {
    format!("{CREDENTIAL_KEY_PREFIX}{provider_id}:{field}")
}

/// All providers and links of a workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecretsConfig {
    pub providers: Vec<ProviderConfig>,
    pub links: Vec<LinkedSecret>,
}

impl SecretsConfig {
    pub fn path(local_dir: &Path) -> PathBuf {
        local_dir.join(CONFIG_FILE)
    }

    /// Reads `<local_dir>/external-secrets.json`; a missing file is an empty
    /// configuration.
    pub fn load(local_dir: &Path) -> Result<Self, SecretError> {
        let path = Self::path(local_dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                SecretError::config(format!("Invalid secret manager settings in {}: {e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SecretsConfig::default()),
            Err(e) => Err(SecretError::config(format!("Could not read {}: {e}", path.display()))),
        }
    }

    pub fn save(&self, local_dir: &Path) -> Result<(), SecretError> {
        let path = Self::path(local_dir);
        std::fs::create_dir_all(local_dir)
            .map_err(|e| SecretError::config(format!("Could not create {}: {e}", local_dir.display())))?;
        let data = serde_json::to_vec_pretty(self).map_err(|e| SecretError::config(e.to_string()))?;
        merpati_core::storage::write_atomic(&path, &data)
            .map_err(|e| SecretError::config(format!("Could not write {}: {e}", path.display())))
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn link(&self, name: &str) -> Option<&LinkedSecret> {
        self.links.iter().find(|l| l.name == name)
    }
}

/// One configured secret manager.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Stable id; credential keys and links refer to it.
    pub id: String,
    /// Display name ("Prod Vault").
    pub name: String,
    #[serde(flatten)]
    pub settings: ProviderSettings,
}

impl ProviderConfig {
    /// A provider with a fresh id.
    pub fn new(name: impl Into<String>, settings: ProviderSettings) -> Self {
        ProviderConfig { id: merpati_core::util::new_id(), name: name.into(), settings }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    HashicorpVault,
    AwsSecretsManager,
    AzureKeyVault,
    OnePassword,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 4] = [
        ProviderKind::HashicorpVault,
        ProviderKind::AwsSecretsManager,
        ProviderKind::AzureKeyVault,
        ProviderKind::OnePassword,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::HashicorpVault => "HashiCorp Vault",
            ProviderKind::AwsSecretsManager => "AWS Secrets Manager",
            ProviderKind::AzureKeyVault => "Azure Key Vault",
            ProviderKind::OnePassword => "1Password",
        }
    }

    /// Settings with defaults, for a "new provider" form.
    pub fn default_settings(self) -> ProviderSettings {
        match self {
            ProviderKind::HashicorpVault => ProviderSettings::HashicorpVault(VaultSettings::default()),
            ProviderKind::AwsSecretsManager => ProviderSettings::AwsSecretsManager(AwsSettings::default()),
            ProviderKind::AzureKeyVault => ProviderSettings::AzureKeyVault(AzureSettings::default()),
            ProviderKind::OnePassword => ProviderSettings::OnePassword(OnePasswordSettings::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSettings {
    HashicorpVault(VaultSettings),
    AwsSecretsManager(AwsSettings),
    AzureKeyVault(AzureSettings),
    OnePassword(OnePasswordSettings),
}

/// A secret input the provider form must show (stored in the secret store).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CredentialField {
    /// Field name for [`credential_key`].
    pub key: &'static str,
    pub label: &'static str,
    pub required: bool,
}

const fn field(key: &'static str, label: &'static str, required: bool) -> CredentialField {
    CredentialField { key, label, required }
}

impl ProviderSettings {
    pub fn kind(&self) -> ProviderKind {
        match self {
            ProviderSettings::HashicorpVault(_) => ProviderKind::HashicorpVault,
            ProviderSettings::AwsSecretsManager(_) => ProviderKind::AwsSecretsManager,
            ProviderSettings::AzureKeyVault(_) => ProviderKind::AzureKeyVault,
            ProviderSettings::OnePassword(_) => ProviderKind::OnePassword,
        }
    }

    /// Secret fields for the current auth mode, in form order.
    pub fn credential_fields(&self) -> Vec<CredentialField> {
        match self {
            ProviderSettings::HashicorpVault(s) => match s.auth {
                VaultAuth::Token => vec![field("token", "Token", true)],
                VaultAuth::AppRole => vec![field("secret_id", "Secret ID", false)],
            },
            ProviderSettings::AwsSecretsManager(s) => match s.credentials {
                AwsCredentialSource::Static => vec![
                    field("access_key_id", "Access key ID", true),
                    field("secret_access_key", "Secret access key", true),
                    field("session_token", "Session token", false),
                ],
                AwsCredentialSource::DefaultChain => Vec::new(),
            },
            ProviderSettings::AzureKeyVault(s) => match s.auth {
                AzureAuth::ClientSecret => vec![field("client_secret", "Client secret", true)],
                AzureAuth::AzureCli => Vec::new(),
            },
            ProviderSettings::OnePassword(s) => match s.mode {
                OnePasswordMode::Cli => vec![field("service_account_token", "Service account token", false)],
                OnePasswordMode::Connect => vec![field("connect_token", "Connect token", true)],
            },
        }
    }

    /// Placeholder for the reference input of a link.
    pub fn reference_hint(&self) -> &'static str {
        match self {
            ProviderSettings::HashicorpVault(_) => "path/to/secret#field  (KV v2: path?version=3#field)",
            ProviderSettings::AwsSecretsManager(_) => "secret-name-or-arn#jsonKey  (or name?versionStage=AWSPREVIOUS)",
            ProviderSettings::AzureKeyVault(_) => "secret-name  (or secret-name/version, secret-name#jsonKey)",
            ProviderSettings::OnePassword(_) => "op://vault/item/field  (or op://vault/item/section/field)",
        }
    }

    /// Checks the settings without network access.
    pub fn validate(&self) -> Result<(), SecretError> {
        match self {
            ProviderSettings::HashicorpVault(s) => {
                check_url("Vault address", &s.address)?;
                if s.auth == VaultAuth::AppRole && s.role_id.trim().is_empty() {
                    return Err(SecretError::config("AppRole login needs a role ID."));
                }
                Ok(())
            }
            ProviderSettings::AwsSecretsManager(s) => {
                if !s.endpoint.trim().is_empty() {
                    check_url("AWS endpoint", &s.endpoint)?;
                }
                Ok(())
            }
            ProviderSettings::AzureKeyVault(s) => {
                check_url("Key Vault URL", &s.vault_url)?;
                if s.auth == AzureAuth::ClientSecret {
                    if s.tenant_id.trim().is_empty() {
                        return Err(SecretError::config("Client credentials need a tenant (directory) ID."));
                    }
                    if s.client_id.trim().is_empty() {
                        return Err(SecretError::config("Client credentials need a client (application) ID."));
                    }
                }
                Ok(())
            }
            ProviderSettings::OnePassword(s) => {
                if s.mode == OnePasswordMode::Connect {
                    check_url("1Password Connect URL", &s.connect_url)?;
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn check_url(what: &str, value: &str) -> Result<url::Url, SecretError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(SecretError::config(format!("{what} is not set.")));
    }
    match url::Url::parse(v) {
        Ok(u) if (u.scheme() == "https" || u.scheme() == "http") && u.host_str().is_some() => Ok(u),
        _ => Err(SecretError::config(format!("{what} \"{v}\" is not a valid http(s) URL."))),
    }
}

/// HashiCorp Vault (KV secrets engine).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultSettings {
    /// `https://vault.example.com:8200`.
    pub address: String,
    /// Vault Enterprise / HCP namespace (`X-Vault-Namespace`); empty for none.
    pub namespace: String,
    pub auth: VaultAuth,
    /// AppRole: role ID (the secret ID goes to the secret store).
    pub role_id: String,
    /// AppRole auth mount path.
    pub approle_mount: String,
    /// KV mount the references are relative to. Empty: the first path
    /// segment of each reference is the mount (`kv/app/db#password`).
    pub mount: String,
    pub kv_version: KvVersion,
}

impl Default for VaultSettings {
    fn default() -> Self {
        VaultSettings {
            address: "http://127.0.0.1:8200".into(),
            namespace: String::new(),
            auth: VaultAuth::Token,
            role_id: String::new(),
            approle_mount: "approle".into(),
            mount: "secret".into(),
            kv_version: KvVersion::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultAuth {
    /// Credential `token`.
    #[default]
    Token,
    /// `role_id` setting plus credential `secret_id`.
    AppRole,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvVersion {
    /// Asks Vault (`sys/internal/ui/mounts`), falling back to v2.
    #[default]
    Auto,
    V1,
    V2,
}

/// AWS Secrets Manager.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AwsSettings {
    /// Empty: `AWS_REGION` / `AWS_DEFAULT_REGION` / the profile's region.
    pub region: String,
    pub credentials: AwsCredentialSource,
    /// Default chain: profile in `~/.aws/credentials` / `~/.aws/config`.
    /// Empty: `AWS_PROFILE` or `default`.
    pub profile: String,
    /// Custom endpoint (VPC endpoint, LocalStack); empty for the regional one.
    pub endpoint: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwsCredentialSource {
    /// Environment variables, then the shared credentials/config files
    /// (static keys or `credential_process`).
    #[default]
    DefaultChain,
    /// Credentials `access_key_id`, `secret_access_key`, optional `session_token`.
    Static,
}

/// Azure Key Vault.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AzureSettings {
    /// `https://my-vault.vault.azure.net`.
    pub vault_url: String,
    pub auth: AzureAuth,
    /// Client credentials: directory (tenant) id. Azure CLI: optional `--tenant`.
    pub tenant_id: String,
    /// Client credentials: application (client) id.
    pub client_id: String,
    /// Microsoft Entra authority; empty for `https://login.microsoftonline.com`.
    pub authority_host: String,
    /// Key Vault REST API version; empty for the default.
    pub api_version: String,
    /// Path to `az`; empty to search `PATH` and the usual install folders.
    pub cli_path: String,
}

impl Default for AzureSettings {
    fn default() -> Self {
        AzureSettings {
            vault_url: String::new(),
            auth: AzureAuth::ClientSecret,
            tenant_id: String::new(),
            client_id: String::new(),
            authority_host: String::new(),
            api_version: String::new(),
            cli_path: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AzureAuth {
    /// Service principal: tenant/client id plus credential `client_secret`.
    #[default]
    ClientSecret,
    /// Token from `az account get-access-token`.
    AzureCli,
}

/// 1Password, through the `op` CLI or a Connect server.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnePasswordSettings {
    pub mode: OnePasswordMode,
    /// CLI: `--account` (sign-in address, email or account id); empty for the default.
    pub account: String,
    /// CLI: path to `op`; empty to search `PATH` and the usual install folders.
    pub cli_path: String,
    /// Connect: server URL (`http://localhost:8080`); credential `connect_token`.
    pub connect_url: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnePasswordMode {
    /// `op read`, using the desktop app integration, an `op signin` session
    /// or the optional credential `service_account_token`.
    #[default]
    Cli,
    Connect,
}

/// A local vault name whose value comes from an external provider:
/// `{{vault:<name>}}` resolves to the secret at `reference`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkedSecret {
    pub name: String,
    pub provider_id: String,
    pub reference: String,
    /// How long a fetched value is reused; 0 fetches on every use.
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
}

impl LinkedSecret {
    pub fn new(name: impl Into<String>, provider_id: impl Into<String>, reference: impl Into<String>) -> Self {
        LinkedSecret {
            name: name.into(),
            provider_id: provider_id.into(),
            reference: reference.into(),
            ttl_secs: DEFAULT_TTL_SECS,
        }
    }

    pub fn with_ttl(mut self, secs: u64) -> Self {
        self.ttl_secs = secs;
        self
    }
}

fn default_ttl() -> u64 {
    DEFAULT_TTL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_and_defaults() {
        let cfg = SecretsConfig {
            providers: vec![
                ProviderConfig {
                    id: "p1".into(),
                    name: "Vault".into(),
                    settings: ProviderSettings::HashicorpVault(VaultSettings::default()),
                },
                ProviderConfig {
                    id: "p2".into(),
                    name: "AWS".into(),
                    settings: ProviderSettings::AwsSecretsManager(AwsSettings {
                        region: "eu-west-1".into(),
                        ..Default::default()
                    }),
                },
            ],
            links: vec![LinkedSecret::new("db", "p1", "app/db#password")],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""kind":"hashicorp_vault""#), "{json}");
        assert_eq!(serde_json::from_str::<SecretsConfig>(&json).unwrap(), cfg);

        let partial: SecretsConfig = serde_json::from_str(
            r#"{"providers":[{"id":"x","name":"op","kind":"one_password"}],"links":[{"name":"a","provider_id":"x","reference":"op://v/i/f"}]}"#,
        )
        .unwrap();
        assert_eq!(partial.links[0].ttl_secs, DEFAULT_TTL_SECS);
        assert_eq!(partial.providers[0].settings, ProviderSettings::OnePassword(OnePasswordSettings::default()));
    }

    #[test]
    fn load_save() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(SecretsConfig::load(dir.path()).unwrap(), SecretsConfig::default());
        let mut cfg = SecretsConfig::default();
        cfg.links.push(LinkedSecret::new("a", "p", "r").with_ttl(5));
        cfg.save(dir.path()).unwrap();
        assert_eq!(SecretsConfig::load(dir.path()).unwrap(), cfg);
    }

    #[test]
    fn validation_and_fields() {
        let mut v = VaultSettings { address: String::new(), ..Default::default() };
        assert!(ProviderSettings::HashicorpVault(v.clone()).validate().is_err());
        v.address = "https://vault:8200".into();
        v.auth = VaultAuth::AppRole;
        assert!(ProviderSettings::HashicorpVault(v.clone()).validate().is_err());
        v.role_id = "r".into();
        let s = ProviderSettings::HashicorpVault(v);
        assert!(s.validate().is_ok());
        assert_eq!(s.credential_fields()[0].key, "secret_id");
        assert_eq!(credential_key("p1", "token"), "extsecrets:p1:token");
        for k in ProviderKind::ALL {
            assert_eq!(k.default_settings().kind(), k);
        }
    }
}
