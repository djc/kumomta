use anyhow::Context;
use cfdkim::DkimPrivateKey;
use config::{from_lua_value, get_or_create_sub_module};
use data_loader::KeySource;
use lruttl::LruCacheWithTtl;
use mlua::prelude::LuaUserData;
use mlua::{Lua, Value};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

lazy_static::lazy_static! {
    static ref SIGNER_CACHE: LruCacheWithTtl<SignerConfig, Arc<CFSigner>> = LruCacheWithTtl::new(1024);
}

#[derive(Deserialize, Hash, Eq, PartialEq, Copy, Clone)]
pub enum Canon {
    Relaxed,
    Simple,
}

impl Default for Canon {
    fn default() -> Self {
        Self::Relaxed
    }
}

#[derive(Deserialize, Hash, Eq, PartialEq, Copy, Clone)]
pub enum HashAlgo {
    Sha1,
    Sha256,
}

#[derive(Deserialize, Hash, PartialEq, Eq, Clone)]
pub struct SignerConfig {
    domain: String,
    selector: String,
    headers: Vec<String>,
    #[serde(default)]
    atps: Option<String>,
    #[serde(default)]
    atpsh: Option<HashAlgo>,
    #[serde(default)]
    agent_user_identifier: Option<String>,
    #[serde(default)]
    expiration: Option<u64>,
    #[serde(default)]
    body_length: bool,
    #[serde(default)]
    reporting: bool,
    #[serde(default)]
    header_canonicalization: Canon,
    #[serde(default)]
    body_canonicalization: Canon,

    key: KeySource,

    #[serde(default = "SignerConfig::default_ttl")]
    ttl: u64,
}

impl SignerConfig {
    fn default_ttl() -> u64 {
        300
    }

    fn configure_cfdkim(&self, key: DkimPrivateKey) -> anyhow::Result<cfdkim::Signer> {
        if self.atps.is_some() {
            anyhow::bail!("atps is not currently supported for RSA keys");
        }
        if self.atpsh.is_some() {
            anyhow::bail!("atpsh is not currently supported for RSA keys");
        }
        if self.agent_user_identifier.is_some() {
            anyhow::bail!("agent_user_identifier is not currently supported for RSA keys");
        }
        if self.body_length {
            anyhow::bail!("body_length is not currently supported for RSA keys");
        }
        if self.reporting {
            anyhow::bail!("reporting is not currently supported for RSA keys");
        }

        let mut signer = cfdkim::SignerBuilder::new()
            .with_signed_headers(&self.headers)
            .context("configure signed headers")?
            .with_private_key(key)
            .with_selector(&self.selector)
            .with_signing_domain(&self.domain)
            .with_header_canonicalization(match self.header_canonicalization {
                Canon::Relaxed => cfdkim::canonicalization::Type::Relaxed,
                Canon::Simple => cfdkim::canonicalization::Type::Simple,
            })
            .with_body_canonicalization(match self.body_canonicalization {
                Canon::Relaxed => cfdkim::canonicalization::Type::Relaxed,
                Canon::Simple => cfdkim::canonicalization::Type::Simple,
            });
        if let Some(exp) = self.expiration {
            signer = signer.with_expiry(chrono::Duration::seconds(exp as i64));
        }

        signer.build().context("build signer")
    }
}

#[derive(Clone)]
pub struct Signer(Arc<CFSigner>);

impl Signer {
    pub fn sign(&self, message: &[u8]) -> anyhow::Result<String> {
        self.0.sign(message)
    }
}

impl LuaUserData for Signer {}

pub fn register<'lua>(lua: &'lua Lua) -> anyhow::Result<()> {
    let dkim_mod = get_or_create_sub_module(lua, "dkim")?;
    dkim_mod.set(
        "rsa_sha256_signer",
        lua.create_async_function(|lua, params: Value| async move {
            let params: SignerConfig = from_lua_value(lua, params)?;

            if let Some(inner) = SIGNER_CACHE.get(&params) {
                return Ok(Signer(inner));
            }

            let data = params
                .key
                .get()
                .await
                .map_err(|err| mlua::Error::external(format!("{:?}: {err:#}", params.key)))?;

            let key = DkimPrivateKey::rsa_key(&data)
                .map_err(|err| mlua::Error::external(format!("{:?}: {err}", params.key)))?;

            let signer = params
                .configure_cfdkim(key)
                .map_err(|err| mlua::Error::external(format!("{err:#}")))?;

            let inner = Arc::new(CFSigner { signer });

            let expiration = Instant::now() + Duration::from_secs(params.ttl);
            SIGNER_CACHE.insert(params, Arc::clone(&inner), expiration);

            Ok(Signer(inner))
        })?,
    )?;

    dkim_mod.set(
        "ed25519_signer",
        lua.create_async_function(|lua, params: Value| async move {
            let params: SignerConfig = from_lua_value(lua, params)?;

            if let Some(inner) = SIGNER_CACHE.get(&params) {
                return Ok(Signer(inner));
            }

            let data = params
                .key
                .get()
                .await
                .map_err(|err| mlua::Error::external(format!("{:?}: {err:#}", params.key)))?;

            let key = DkimPrivateKey::ed25519_key(&data)
                .map_err(|err| mlua::Error::external(format!("{:?}: {err}", params.key)))?;

            let signer = params
                .configure_cfdkim(key)
                .map_err(|err| mlua::Error::external(format!("{err:#}")))?;

            let inner = Arc::new(CFSigner { signer });

            let expiration = Instant::now() + Duration::from_secs(params.ttl);
            SIGNER_CACHE.insert(params, Arc::clone(&inner), expiration);

            Ok(Signer(inner))
        })?,
    )?;
    Ok(())
}

pub struct CFSigner {
    signer: cfdkim::Signer,
}

impl CFSigner {
    fn sign(&self, message: &[u8]) -> anyhow::Result<String> {
        let mail = cfdkim::ParsedEmail::parse_bytes(&message)
            .ok_or_else(|| anyhow::anyhow!("failed to parse message to pass to dkim signer"))?;

        let dkim_header = self.signer.sign(&mail)?;

        Ok(dkim_header)
    }
}
