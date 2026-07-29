mod registry;

pub use registry::FeatureRegistry;
pub(crate) use registry::{initial_webrtc_capabilities, is_initial_webrtc_feature};
