use super::Dac;
use crate::routing::{
    ControlKey, RouteEdit, RouteReport, RouteValue, RoutingSessionError, RoutingSnapshot,
};
use crate::transport::routing_hw;

    /// Unavailable in this release; retained for source compatibility.
    ///
impl Dac {
    /// Discover source-backed DJM choices on an exact, idle vendor PCM session.
    /// Discovery leaves the mixer's routing intact.
    pub fn djm_routing_discover(
        &self,
    ) -> Result<crate::pioneer::DjmRoutingProfile, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::djm_discover(&self.shared)
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Send one explicitly selected DJM option after fresh model, configuration,
    /// claim and idle-alternate proof. `option` is a listed zero-based option ID.
    /// Success confirms USB acknowledgement. The mixer's current routing
    /// remains unknown through this protocol.
    pub fn djm_routing_apply(
        &self,
        model: crate::pioneer::DjmModel,
        configuration: u8,
        key: crate::pioneer::DjmControlKey,
        option: u8,
    ) -> Result<(), RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::djm_apply(&self.shared, model, configuration, key, option)
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Read source-backed first-generation Focusrite controls using the already
    /// claimed AudioControl interface while idle. No vendor claim or probe write.
    /// Hardware behavior remains physically unvalidated.
    pub fn scarlett_gen1_read(&self) -> Result<crate::scarlett::Gen1Readback, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_gen1_read()
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Apply bounded typed Gen1 controls against a fresh readback on the live DAC.
    /// Verify each write and restore attempted controls on failure. Configured
    /// hand-back preserves external edits; no clock, preamp or persistent writes.
    pub fn scarlett_gen1_apply(&self, expected: &crate::scarlett::Gen1Readback,
        edits: &[crate::scarlett::Gen1Edit]) -> Result<crate::scarlett::Gen1Report, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_gen1_apply(expected, edits)
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
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

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Initialize and read source-backed Scarlett routing while idle. Requires
    /// the host's explicit vendor-interface claim. Exact-model firmware and live geometry
    /// guards apply before mux/mix controls become available. No route, volume or persistent setting
    /// writes are sent. Hardware behavior remains unvalidated.
    pub fn scarlett_routing_read(
        &self,
    ) -> Result<crate::scarlett::RoutingReadback, RoutingSessionError> {
        self.shared
            .ensure_open()
            .map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_read()
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Read firmware-defined modern Focusrite FCP pins, mux banks and mix rows.
    /// Requires idle ownership and an explicit exact-model vendor-interface claim.
    /// Does not send persistent configuration commands.
    pub fn fcp_routing_read(&self) -> Result<crate::scarlett::FcpReadback, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::fcp_read()
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Apply typed firmware pin routes and mixer coefficients against a fresh readback.
    /// Requires idle ownership and explicit vendor claim. Verify every affected target,
    /// restore attempted targets on failure, and preserve external edits at hand-back.
    /// No persistent configuration commands are sent. Physically unvalidated.
    pub fn fcp_routing_apply(&self, expected: &crate::scarlett::FcpReadback,
        routes: &[crate::scarlett::FcpRouteEdit], mixes: &[crate::scarlett::FcpMixEdit])
        -> Result<crate::scarlett::FcpWriteReport, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::fcp_apply(expected, routes, mixes)
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
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

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Probe optional per-bank support and read every raw bank including padding.
    /// An unsupported response never enables writes; legacy reads remain usable.
    pub fn scarlett_mux_read(&self) -> Result<crate::scarlett::MuxSnapshot, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_mux_read()
    }

    /// Unavailable in this release; retained for source compatibility.
    ///
    /// Apply explicit bank edits only after a fresh complete capability/geometry
    /// read equals the expected snapshot. Restore each attempted raw bank after
    /// any uncertain write. Configured hand-back preserves external edits.
    /// Source-backed; physical routing has not been validated.
    pub fn scarlett_mux_apply(&self, edit: &crate::scarlett::MuxEdit)
        -> Result<crate::scarlett::MuxWriteReport, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::scarlett_mux_apply(edit)
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

    /// Explicitly wake affected full ADC3 routing domains on entirely claimed
    /// idle functions. Restore checked original power after discovery, including
    /// failures. The snapshot does not retain a power lease.
    pub fn routing_discover_powered(&self) -> Result<RoutingSnapshot, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::discover_with_power(true)
    }

    /// Revalidate the snapshot and acquire a fresh scoped ADC3 routing lease.
    /// Preserve external power drift instead of overwriting it on hand-back.
    pub fn routing_read_powered(&self, snapshot: &RoutingSnapshot, key: ControlKey)
        -> Result<RouteValue, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::read_with_power(snapshot, key, true)
    }

    /// Apply the ordinary verified routing transaction with explicit scoped
    /// ADC3 wake. Restore original power after success or failure. Configured
    /// route hand-back reacquires power only for these opted-in edits.
    pub fn routing_apply_powered(&self, snapshot: &RoutingSnapshot, edits: &[RouteEdit])
        -> Result<RouteReport, RoutingSessionError> {
        self.shared.ensure_open().map_err(RoutingSessionError::Session)?;
        routing_hw::apply_with_power(snapshot, edits, true)
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
