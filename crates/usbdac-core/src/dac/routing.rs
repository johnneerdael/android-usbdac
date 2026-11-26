use super::Dac;
use crate::routing::{
    ControlKey, RouteEdit, RouteReport, RouteValue, RoutingSessionError, RoutingSnapshot,
};
use crate::transport::routing_hw;

impl Dac {
    /// Discover standard UAC2 selectors and mixer crosspoints on claimed
    /// AudioControl interfaces in the active configuration. No vendor protocol
    /// is inferred. Requires an idle DAC, including no open capture stream.
    pub fn routing_discover(&self) -> Result<RoutingSnapshot, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::discover()
    }

    /// Read a descriptor-backed control after checking the attachment,
    /// configuration, descriptors and discovered selector pins for drift.
    pub fn routing_read(
        &self,
        snapshot: &RoutingSnapshot,
        key: ControlKey,
    ) -> Result<RouteValue, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::read(snapshot, key)
    }

    /// Apply bounded, verified routing edits only while idle. Each edit includes
    /// the expected current value from `routing_read`; stale values fail before
    /// writing. Rediscover after changing a selector. The configured HandBack
    /// policy controls route restoration on close; `close_with` overrides volume
    /// handling only. Feature Unit volume and mute settings are not changed.
    pub fn routing_apply(
        &self,
        snapshot: &RoutingSnapshot,
        edits: &[RouteEdit],
    ) -> Result<RouteReport, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::apply(snapshot, edits)
    }
}
