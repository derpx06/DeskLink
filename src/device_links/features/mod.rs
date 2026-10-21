mod registry;

pub use registry::FeatureRegistry;
pub(crate) use registry::{
    handover_list_is_consistent, is_webrtc_feature, peer_accepts, peer_sends, record_peer_profile,
    webrtc_capabilities,
};
