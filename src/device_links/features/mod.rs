mod registry;

pub use registry::FeatureRegistry;
pub(crate) use registry::{is_webrtc_feature, webrtc_capabilities};
