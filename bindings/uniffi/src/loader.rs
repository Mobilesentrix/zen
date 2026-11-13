use crate::error::ZenError;
use crate::types::JsonBuffer;
use ms_zen_database::{InstanceRouting, prepare_decision_content};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use uniffi::deps::anyhow::anyhow;
use zen_engine::loader::{
    DecisionLoader, DynamicLoader, FilesystemLoader, FilesystemLoaderOptions, LoaderConfig,
    LoaderError, LoaderResponse, MemoryLoader,
};
use zen_engine::model::DecisionContent;

#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait ZenDecisionLoaderCallback: Send + Sync {
    async fn load(&self, key: String) -> Result<Option<JsonBuffer>, ZenError>;
}

pub struct NoopDecisionLoader;

#[async_trait::async_trait]
impl ZenDecisionLoaderCallback for NoopDecisionLoader {
    async fn load(&self, _: String) -> Result<Option<JsonBuffer>, ZenError> {
        Err(ZenError::Zero)
    }
}

#[derive(uniffi::Enum)]
pub enum ZenLoader {
    Callback {
        callback: Arc<dyn ZenDecisionLoaderCallback>,
    },
    Static {
        content: HashMap<String, JsonBuffer>,
    },
    Filesystem {
        path: String,
    },
    Zip {
        bytes: Vec<u8>,
    },
}

/// How a loader turns decision JSON into decision content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentReader {
    /// The engine's own format only.
    Plain,
    /// Also rules saved by the Rules Engine editor: Lookup steps (`databaseNode`) become the
    /// engine's database node, routed as given.
    Editor(InstanceRouting),
}

impl ContentReader {
    pub fn read(self, json: &[u8]) -> Result<DecisionContent, uniffi::deps::anyhow::Error> {
        match self {
            ContentReader::Plain => Ok(serde_json::from_slice(json)?),
            ContentReader::Editor(routing) => Ok(prepare_decision_content(json, routing)?),
        }
    }
}

impl ZenLoader {
    pub fn into_dynamic_loader_with(
        self,
        reader: ContentReader,
    ) -> Result<DynamicLoader, ZenError> {
        if let ContentReader::Editor(_) = reader {
            return self.into_editor_loader(reader);
        }
        let config = match self {
            ZenLoader::Callback { callback } => {
                return Ok(Arc::new(ZenDecisionLoaderCallbackWrapper::new(
                    callback, reader,
                )));
            }
            ZenLoader::Static { content } => {
                let content = content
                    .into_iter()
                    .map(|(key, buffer)| {
                        let decision_content: DecisionContent =
                            serde_json::from_slice(buffer.0.as_slice())
                                .map_err(|_| ZenError::JsonDeserializationFailed)?;
                        Ok((key, decision_content))
                    })
                    .collect::<Result<HashMap<_, _>, ZenError>>()?;

                LoaderConfig::Static { content }
            }
            ZenLoader::Filesystem { path } => LoaderConfig::Filesystem { path },
            ZenLoader::Zip { bytes } => LoaderConfig::Zip { bytes },
        };

        config
            .into_loader()
            .map_err(|e| ZenError::ValidationError(e.to_string()))
    }

    /// Every kind of loader with each decision read through `reader`.
    fn into_editor_loader(self, reader: ContentReader) -> Result<DynamicLoader, ZenError> {
        match self {
            ZenLoader::Callback { callback } => Ok(Arc::new(
                ZenDecisionLoaderCallbackWrapper::new(callback, reader),
            )),
            ZenLoader::Static { content } => {
                let loader = MemoryLoader::default();
                for (key, buffer) in content {
                    let decision =
                        reader
                            .read(&buffer.0)
                            .map_err(|e| ZenError::LoaderInternalError {
                                key: key.clone(),
                                details: e.to_string(),
                            })?;
                    loader.add(key, decision);
                }
                Ok(Arc::new(loader))
            }
            ZenLoader::Filesystem { path } => Ok(Arc::new(ReadingFilesystemLoader {
                keys: FilesystemLoader::new(FilesystemLoaderOptions { root: path.clone() }),
                root: path,
                reader,
            })),
            ZenLoader::Zip { bytes } => {
                zip_loader(&bytes, reader).map_err(|e| ZenError::ValidationError(e.to_string()))
            }
        }
    }
}

/// The `.json` entries of a zip archive, keyed by their names (as the engine's zip loader).
fn zip_loader(
    bytes: &[u8],
    reader: ContentReader,
) -> Result<DynamicLoader, uniffi::deps::anyhow::Error> {
    use std::io::Read;

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let loader = MemoryLoader::default();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if !entry.is_file() || !entry.name().ends_with(".json") {
            continue;
        }
        let key = entry.name().to_string();
        let mut buffer = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buffer)?;
        let decision = reader.read(&buffer).map_err(|e| anyhow!("{key}: {e}"))?;
        loader.add(key, decision);
    }
    Ok(Arc::new(loader))
}

/// Decisions read from files under `root` through a [`ContentReader`]; key listing as the
/// engine's own filesystem loader.
#[derive(Debug)]
struct ReadingFilesystemLoader {
    keys: FilesystemLoader,
    root: String,
    reader: ContentReader,
}

impl ReadingFilesystemLoader {
    fn read_content(&self, key: &str) -> LoaderResponse {
        let path = Path::new(&self.root).join(key);
        if !path.exists() {
            return Err(LoaderError::NotFound(key.to_string()));
        }
        let internal = |source: uniffi::deps::anyhow::Error| LoaderError::Internal {
            key: key.to_string(),
            source,
        };
        let bytes = std::fs::read(&path).map_err(|e| internal(e.into()))?;
        let decision = self.reader.read(&bytes).map_err(internal)?;
        Ok(Arc::new(decision))
    }
}

impl DecisionLoader for ReadingFilesystemLoader {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = LoaderResponse> + 'a + Send>> {
        Box::pin(async move { self.read_content(key) })
    }

    fn load_sync(&self, key: &str) -> Option<LoaderResponse> {
        Some(self.read_content(key))
    }

    fn keys(&self) -> Option<Vec<Arc<str>>> {
        self.keys.keys()
    }
}

pub struct ZenDecisionLoaderCallbackWrapper {
    callback: Arc<dyn ZenDecisionLoaderCallback>,
    reader: ContentReader,
}

impl ZenDecisionLoaderCallbackWrapper {
    pub fn new(callback: Arc<dyn ZenDecisionLoaderCallback>, reader: ContentReader) -> Self {
        Self { callback, reader }
    }
}

impl Debug for ZenDecisionLoaderCallbackWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ZenDecisionLoaderCallbackWrapper")
    }
}

impl DecisionLoader for ZenDecisionLoaderCallbackWrapper {
    fn load<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = LoaderResponse> + 'a + Send>> {
        Box::pin(async move {
            let maybe_json_buffer = match self.callback.load(key.into()).await {
                Ok(r) => r,
                Err(error) => {
                    return Err(LoaderError::Internal {
                        key: key.to_string(),
                        source: anyhow!(error),
                    });
                }
            };

            let Some(json_buffer) = maybe_json_buffer else {
                return Err(LoaderError::NotFound(key.to_string()));
            };

            let decision_content =
                self.reader
                    .read(json_buffer.0.as_slice())
                    .map_err(|source| LoaderError::Internal {
                        key: key.to_string(),
                        source,
                    })?;

            Ok(Arc::new(decision_content))
        })
    }
}
