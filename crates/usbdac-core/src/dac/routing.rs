use super::Dac;
use crate::routing::{
    ControlKey, RouteEdit, RouteReport, RouteValue, RoutingSessionError, RoutingSnapshot,
};
use crate::transport::routing_hw;

impl Dac {
    /// Inspect the exact Scarlett read candidate and required vendor claim,
    /// without initializing its protocol or reading proprietary state.
    pub fn scarlett_routing_capability(
        &self,
    ) -> Result<Option<crate::scarlett::ReadCapability>, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_discover()
    }

    /// Initialize and read source-backed Scarlett routing while idle. Requires
    /// the host's explicit vendor-interface claim. Only 1235:8201 firmware1083
    /// is admitted for mux/mix queries. No route, volume or persistent setting
    /// writes are sent. Hardware behavior remains unvalidated.
    pub fn scarlett_routing_read(
        &self,
    ) -> Result<crate::scarlett::RoutingReadback, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_read()
    }

    /// Apply bounded half-dB replacements to one complete Scarlett mixer row.
    /// Requires fresh expected raw coefficients, idle ownership and the explicit
    /// vendor claim. Verifies the whole row and attempts verified restoration
    /// after any attempted-write failure. Configured hand-back restores only
    /// rows still matching the last verified write. Source-backed, physically
    /// unverified; no mux bank or persistent configuration is written.
    pub fn scarlett_mix_apply(
        &self,
        edit: &crate::scarlett::MixRowEdit,
    ) -> Result<crate::scarlett::MixWriteReport, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_mix_apply(edit)
    }

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
