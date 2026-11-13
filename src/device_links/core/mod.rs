mod device_manager;
mod device_session;
pub mod link_transport;
mod session_link;
#[cfg(test)]
pub(crate) mod test_link;

#[allow(unused_imports)]
pub use device_manager::{
    Adoption, DeviceConnection, DeviceManager, DisconnectResult, FeatureTransportSnapshot,
    Installed, LivenessTick, ReconnectLease, RegistrationResult, SessionError,
};
pub use device_session::{
    ConnectionGeneration, DeviceSession, ReplayCache, SessionBinding, SessionId, SessionState,
};
#[cfg(test)]
pub use device_session::{PENDING_FEATURE_LIMIT, PENDING_FEATURE_MAX_AGE};
pub use link_transport::{LinkTransport, ReadOutcome};
#[cfg(test)]
pub use session_link::LINK_RACE_WINDOW;
pub use session_link::{link_race, RaceVerdict, SessionLink};
