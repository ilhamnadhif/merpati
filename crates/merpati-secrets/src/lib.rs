//! External secret managers for Merpati vault variables (PRD section 4,
//! "Variabel", phase 3): HashiCorp Vault, AWS Secrets Manager, Azure Key
//! Vault and 1Password.
//!
//! A *linked vault secret* maps a local vault name to a provider reference,
//! so `{{vault:name}}` transparently returns the external value:
//!
//! - [`SecretsConfig`] (providers + links, no secrets) is stored in the
//!   workspace local folder as `external-secrets.json`.
//! - Credentials are stored in the workspace [`SecretStore`] under
//!   [`credential_key`]`(provider_id, field)` (`extsecrets:<id>:<field>`).
//! - [`ExternalSecrets`] implements [`VaultLookup`]: linked names resolve
//!   through the provider (with an in-memory TTL cache), other names from
//!   the local vault. A link shadows a local value with the same name.
//!
//! Secret values are never logged, written to disk or put in error
//! messages, and `Debug` output of credential-carrying types is redacted.
//!
//! [`SecretStore`]: merpati_core::secrets::SecretStore
//! [`VaultLookup`]: merpati_core::variables::VaultLookup

mod cli;
mod env;
mod error;
mod manager;
mod model;
mod providers;
mod reference;

pub use error::{ErrorKind, SecretError};
pub use manager::{
    CachedLookup, DEFAULT_ERROR_BACKOFF, DEFAULT_SYNC_TIMEOUT, ExternalSecrets, LinkStatus, client_for, default_client,
    vault_references,
};
pub use model::{
    AwsCredentialSource, AwsSettings, AzureAuth, AzureSettings, CONFIG_FILE, CREDENTIAL_KEY_PREFIX, CredentialField,
    DEFAULT_TTL_SECS, KvVersion, LinkedSecret, OnePasswordMode, OnePasswordSettings, ProviderConfig, ProviderKind,
    ProviderSettings, SecretsConfig, VaultAuth, VaultSettings, credential_key,
};
pub use providers::{BrowseEntry, EntryKind};

#[cfg(test)]
mod tests;
