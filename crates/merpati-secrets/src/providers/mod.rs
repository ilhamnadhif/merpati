//! Provider implementations behind one dispatch point.

pub(crate) mod aws;
pub(crate) mod azure;
pub(crate) mod hashicorp;
pub(crate) mod onepassword;

use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::error::SecretError;
use crate::model::{ProviderConfig, ProviderSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A folder, mount, vault or item: browse `path` to go deeper.
    Folder,
    /// A secret that can be linked as-is (and may have fields to browse).
    Secret,
    /// A field or key inside a secret.
    Field,
}

/// One row of a provider browser. Never contains a secret value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Pass to `browse` to list the children, when there are any.
    pub path: Option<String>,
    /// Reference to store in a link, when the entry can be linked.
    pub reference: Option<String>,
}

impl BrowseEntry {
    pub(crate) fn folder(name: impl Into<String>, path: impl Into<String>) -> Self {
        BrowseEntry { name: name.into(), kind: EntryKind::Folder, path: Some(path.into()), reference: None }
    }

    pub(crate) fn secret(name: impl Into<String>, reference: impl Into<String>, browsable: bool) -> Self {
        let reference = reference.into();
        BrowseEntry {
            name: name.into(),
            kind: EntryKind::Secret,
            path: browsable.then(|| reference.clone()),
            reference: Some(reference),
        }
    }

    pub(crate) fn field(name: impl Into<String>, reference: impl Into<String>) -> Self {
        BrowseEntry { name: name.into(), kind: EntryKind::Field, path: None, reference: Some(reference.into()) }
    }
}

impl ProviderSettings {
    /// Checks the syntax of a reference for this provider, without network access.
    pub fn check_reference(&self, reference: &str) -> Result<(), SecretError> {
        match self {
            ProviderSettings::HashicorpVault(s) => hashicorp::check_reference(s, reference),
            ProviderSettings::AwsSecretsManager(_) => aws::check_reference(reference),
            ProviderSettings::AzureKeyVault(_) => azure::check_reference(reference),
            ProviderSettings::OnePassword(_) => onepassword::check_reference(reference),
        }
    }
}

pub(crate) async fn fetch(env: &Env, p: &ProviderConfig, reference: &str) -> Result<String, SecretError> {
    p.settings.validate()?;
    match &p.settings {
        ProviderSettings::HashicorpVault(s) => hashicorp::fetch(env, &p.id, s, reference).await,
        ProviderSettings::AwsSecretsManager(s) => aws::fetch(env, &p.id, s, reference).await,
        ProviderSettings::AzureKeyVault(s) => azure::fetch(env, &p.id, s, reference).await,
        ProviderSettings::OnePassword(s) => onepassword::fetch(env, &p.id, s, reference).await,
    }
}

pub(crate) async fn test(env: &Env, p: &ProviderConfig) -> Result<String, SecretError> {
    p.settings.validate()?;
    match &p.settings {
        ProviderSettings::HashicorpVault(s) => hashicorp::test(env, &p.id, s).await,
        ProviderSettings::AwsSecretsManager(s) => aws::test(env, &p.id, s).await,
        ProviderSettings::AzureKeyVault(s) => azure::test(env, &p.id, s).await,
        ProviderSettings::OnePassword(s) => onepassword::test(env, &p.id, s).await,
    }
}

pub(crate) async fn browse(env: &Env, p: &ProviderConfig, path: &str) -> Result<Vec<BrowseEntry>, SecretError> {
    p.settings.validate()?;
    let mut entries = match &p.settings {
        ProviderSettings::HashicorpVault(s) => hashicorp::browse(env, &p.id, s, path).await?,
        ProviderSettings::AwsSecretsManager(s) => aws::browse(env, &p.id, s, path).await?,
        ProviderSettings::AzureKeyVault(s) => azure::browse(env, &p.id, s, path).await?,
        ProviderSettings::OnePassword(s) => onepassword::browse(env, &p.id, s, path).await?,
    };
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Folder, a.name.to_lowercase()).cmp(&(b.kind != EntryKind::Folder, b.name.to_lowercase()))
    });
    Ok(entries)
}
