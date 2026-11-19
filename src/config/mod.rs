//! Katalog referensi model & provider, discovery `/models`, serta single source of truth
//! penyimpanan konfigurasi (`auth.json`, `models.json`, `settings.json`, `~/.config/oopbot/.env`).
//!
//! - [`catalog`]: [`ModelInfo`], [`ModelCost`], [`ModelCatalog`], [`ProviderCatalogEntry`], glob scope.
//! - [`builtin_providers`]: daftar provider & model bawaan (`builtin_providers`, `builtin_catalog_json`).
//! - [`discovery`]: parser respons `/models` & inferensi kapabilitas (`parse_models_response`, `infer_*`).
//! - [`store`]: baca/tulis terpusat `auth.json` (`0o600`), `models.json`, `settings.json`, dan `.env`.

mod builtin_providers;
mod catalog;
mod discovery;
pub(crate) mod store;

#[cfg(test)]
mod tests;

pub use builtin_providers::{builtin_catalog_json, builtin_providers};
pub use catalog::{
    glob_match, resolve_model_scope, ModelCatalog, ModelCost, ModelInfo, ProviderCatalogEntry,
    CANONICAL_THINKING_LEVELS,
};
pub use discovery::{
    infer_api_from_provider, infer_input_from_id, infer_reasoning_from_id, parse_models_response,
};
pub use store::extract_auth_credentials;
