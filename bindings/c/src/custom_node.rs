use crate::languages::native::NativeCustomNode;
use crate::ms_alloc::take_malloc_c_string;
use anyhow::anyhow;
use std::ffi::c_char;
use std::future::Future;
use std::pin::Pin;
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest, NoopCustomNode};
use zen_engine::nodes::{NodeResponse, NodeResult};

#[derive(Debug)]
pub(crate) enum DynamicCustomNode {
    Noop(NoopCustomNode),
    Native(NativeCustomNode),
    /// MobileSentrix database node (kind "database") in front of a fallback adapter.
    Database(ms_zen_database::DatabaseCustomNode),
    #[cfg(feature = "go")]
    Go(crate::languages::go::GoCustomNode),
}

impl Default for DynamicCustomNode {
    fn default() -> Self {
        Self::Noop(Default::default())
    }
}

impl CustomNodeAdapter for DynamicCustomNode {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        Box::pin(async move {
            match self {
                DynamicCustomNode::Noop(cn) => cn.handle(request).await,
                DynamicCustomNode::Native(cn) => cn.handle(request).await,
                DynamicCustomNode::Database(cn) => cn.handle(request).await,
                #[cfg(feature = "go")]
                DynamicCustomNode::Go(cn) => cn.handle(request).await,
            }
        })
    }
}

#[repr(C)]
pub struct ZenCustomNodeResult {
    content: *mut c_char,
    error: *mut c_char,
}

impl ZenCustomNodeResult {
    pub fn into_node_result(self) -> anyhow::Result<NodeResponse> {
        // The callback allocated both strings with malloc (MS: freed with free, not with the
        // Rust allocator; a content sent alongside an error is freed too instead of leaking).
        let maybe_error = match self.error.is_null() {
            false => Some(unsafe { take_malloc_c_string(self.error) }),
            true => None,
        };
        let maybe_content = match self.content.is_null() {
            false => Some(unsafe { take_malloc_c_string(self.content) }),
            true => None,
        };

        if let Some(c_error) = maybe_error {
            let maybe_str = c_error.to_str().unwrap_or("unknown error");
            return Err(anyhow!("{maybe_str}"));
        }

        let Some(content_cstr) = maybe_content else {
            return Err(anyhow!("response not provided"));
        };

        let node_response: NodeResponse = serde_json::from_slice(content_cstr.to_bytes())
            .map_err(|_| anyhow!("failed to deserialize"))?;

        Ok(node_response)
    }
}

#[cfg(test)]
impl ZenCustomNodeResult {
    /// Test helper: a callback that provides nothing (graphs under test have no custom nodes).
    pub(crate) fn none() -> Self {
        Self {
            content: std::ptr::null_mut(),
            error: std::ptr::null_mut(),
        }
    }
}
