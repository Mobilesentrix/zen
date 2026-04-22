use serde::{Deserialize, Serialize};
use serde_with::{DurationMilliSeconds, serde_as};
use std::time::Duration;

#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionOptions {
    pub max_connections: Option<u32>,
    pub min_connections: Option<u32>,
    #[serde_as(as = "Option<DurationMilliSeconds<u64>>")]
    pub acquire_timeout: Option<Duration>,
    #[serde_as(as = "Option<DurationMilliSeconds<u64>>")]
    pub idle_timeout: Option<Duration>,
    #[serde_as(as = "Option<DurationMilliSeconds<u64>>")]
    pub max_lifetime: Option<Duration>,
}
