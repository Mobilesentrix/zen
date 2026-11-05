use std::ffi::c_char;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::anyhow;

use zen_engine::loader::{
    DecisionLoader, DynamicLoader, LoaderConfig, LoaderError, LoaderResponse, NoopLoader,
};
use zen_engine::model::DecisionContent;

use crate::error::ZenError;
use crate::helper::safe_str_from_ptr;
use crate::languages::native::NativeDecisionLoader;

#[derive(Debug)]
pub(crate) enum DynamicDecisionLoader {
    Noop(NoopLoader),
    Native(NativeDecisionLoader),
    Config(DynamicLoader),
    #[cfg(feature = "go")]
    Go(crate::languages::go::GoDecisionLoader),
}

impl Default for DynamicDecisionLoader {
    fn default() -> Self {
        Self::Noop(Default::default())
    }
}

impl DynamicDecisionLoader {
    /// A loader that calls back into the embedder (Go / native C). It cannot enumerate its
    /// keys, so the engine's compiled set never covers it; `ZenEngine` caches what it returns.
    pub(crate) fn is_callback(&self) -> bool {
        match self {
            DynamicDecisionLoader::Native(_) => true,
            #[cfg(feature = "go")]
            DynamicDecisionLoader::Go(_) => true,
            DynamicDecisionLoader::Noop(_) | DynamicDecisionLoader::Config(_) => false,
        }
    }
}

impl DecisionLoader for DynamicDecisionLoader {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = LoaderResponse> + Send + 'a>> {
        Box::pin(async move {
            match self {
                DynamicDecisionLoader::Noop(loader) => loader.load(key).await,
                DynamicDecisionLoader::Native(loader) => loader.load(key).await,
                DynamicDecisionLoader::Config(loader) => loader.load(key).await,
                #[cfg(feature = "go")]
                DynamicDecisionLoader::Go(loader) => loader.load(key).await,
            }
        })
    }

    // `keys` and `load_sync` are what `DecisionEngine::compile()` builds the compiled set
    // from; a wrapper that hides them silently disables the cache for every engine.
    fn keys(&self) -> Option<Vec<Arc<str>>> {
        match self {
            DynamicDecisionLoader::Config(loader) => loader.keys(),
            _ => None,
        }
    }

    fn load_sync(&self, key: &str) -> Option<LoaderResponse> {
        match self {
            DynamicDecisionLoader::Config(loader) => loader.load_sync(key),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[repr(C)]
pub enum ZenLoaderConfigKind {
    Static,
    Filesystem,
    Zip,
}

#[repr(C)]
pub struct ZenEngineLoaderConfig {
    pub(crate) kind: ZenLoaderConfigKind,
    pub(crate) content: *const c_char,
    pub(crate) bytes: *const u8,
    pub(crate) bytes_len: usize,
}

impl ZenEngineLoaderConfig {
    pub(crate) fn to_dynamic_loader(&self) -> Result<DynamicLoader, ZenError> {
        let config = match self.kind {
            ZenLoaderConfigKind::Static => {
                let content = safe_str_from_ptr(self.content).ok_or(ZenError::InvalidArgument)?;
                LoaderConfig::Static {
                    content: serde_json::from_str(content)
                        .map_err(|_| ZenError::JsonDeserializationFailed)?,
                }
            }
            ZenLoaderConfigKind::Filesystem => {
                let path = safe_str_from_ptr(self.content).ok_or(ZenError::InvalidArgument)?;
                LoaderConfig::Filesystem {
                    path: path.to_string(),
                }
            }
            ZenLoaderConfigKind::Zip => {
                if self.bytes.is_null() {
                    return Err(ZenError::InvalidArgument);
                }

                let bytes =
                    unsafe { std::slice::from_raw_parts(self.bytes, self.bytes_len) }.to_vec();
                LoaderConfig::Zip { bytes }
            }
        };

        config
            .into_loader()
            .map_err(|e| ZenError::LoaderConfigError {
                message: e.to_string(),
            })
    }
}

#[repr(C)]
pub struct ZenDecisionLoaderResult {
    content: *mut c_char,
    error: *mut c_char,
}

impl ZenDecisionLoaderResult {
    pub fn into_loader_response(self, key: &str) -> LoaderResponse {
        let maybe_error = match self.error.is_null() {
            false => Some(unsafe { crate::ms_alloc::take_malloc_c_string(self.error) }),
            true => None,
        };

        if let Some(c_error) = maybe_error {
            return Err(LoaderError::Internal {
                key: key.to_string(),
                source: anyhow!(c_error.to_string_lossy().to_string()),
            }
            .into());
        }

        let maybe_content = match self.content.is_null() {
            false => Some(unsafe { crate::ms_alloc::take_malloc_c_string(self.content) }),
            true => None,
        };

        // If both pointers are null, we are treating it as not found
        let Some(c_content) = maybe_content else {
            return Err(LoaderError::NotFound(key.to_string()).into());
        };

        let decision_content: DecisionContent = serde_json::from_slice(c_content.to_bytes())
            .map_err(|e| LoaderError::Internal {
                key: key.to_string(),
                source: anyhow!(e),
            })?;

        Ok(Arc::new(decision_content))
    }
}

#[cfg(test)]
impl ZenDecisionLoaderResult {
    /// Test helper standing in for a native/Go loader callback returning `content`.
    pub(crate) fn from_content(content: &str) -> Self {
        Self {
            content: crate::ms_alloc::malloc_c_string(content.as_bytes()),
            error: std::ptr::null_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_loader_exposes_keys_and_sync_loads_through_the_dynamic_wrapper() {
        let content = r#"{"g.json": {"nodes": [], "edges": []}}"#;
        let loader = LoaderConfig::Static {
            content: serde_json::from_str(content).unwrap(),
        }
        .into_loader()
        .unwrap();
        let dynamic = DynamicDecisionLoader::Config(loader);
        assert_eq!(dynamic.keys().unwrap(), vec![Arc::from("g.json")]);
        assert!(dynamic.load_sync("g.json").unwrap().is_ok());
        assert!(dynamic.load_sync("missing.json").unwrap().is_err());
        assert!(DynamicDecisionLoader::default().keys().is_none());
        assert!(DynamicDecisionLoader::default()
            .load_sync("g.json")
            .is_none());
    }
}
