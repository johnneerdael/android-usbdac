use rusb::{SyncType, TransferType, UsageType};

#[derive(Debug, Clone, Copy)]
pub(crate) struct FeedbackEndpointCandidate {
    pub alt_setting: u8,
    pub address: u8,
    pub transfer_type: TransferType,
    pub usage_type: UsageType,
    pub interval: u8,
    pub max_packet_size: u16,
}

/// Select explicit feedback within one interface, only from the data endpoint's
/// selected alternate setting. Repeated addresses in inactive alts are not ambiguity.
pub(crate) fn select_explicit_feedback_endpoint(
    selected_alt: u8,
    synch_address: u8,
    sync_type: SyncType,
    endpoints: impl IntoIterator<Item = FeedbackEndpointCandidate>,
) -> Result<Option<FeedbackEndpointCandidate>, Vec<u8>> {
    let endpoints: Vec<_> = endpoints
        .into_iter()
        .filter(|endpoint| {
            endpoint.alt_setting == selected_alt
                && (endpoint.address & 0x80) != 0
                && matches!(
                    endpoint.transfer_type,
                    TransferType::Isochronous | TransferType::Interrupt
                )
                && endpoint.interval > 0
                && (endpoint.max_packet_size & 0x07ff) >= 3
        })
        .collect();

    // UAC1 and some UAC2 devices link the feedback endpoint explicitly.
    if synch_address != 0 {
        if let Some(endpoint) = endpoints
            .iter()
            .find(|endpoint| endpoint.address == synch_address)
        {
            return Ok(Some(*endpoint));
        }
    }

    if sync_type != SyncType::Asynchronous {
        return Ok(None);
    }

    // rusb maps usage 0b01 to Feedback, but 0b10 (FeedbackData) carries
    // implicit-feedback audio, not a fixed-point explicit clock report.
    let candidates: Vec<_> = endpoints
        .into_iter()
        .filter(|endpoint| endpoint.usage_type == UsageType::Feedback)
        .collect();
    match candidates.as_slice() {
        [] => Ok(None),
        [endpoint] => Ok(Some(*endpoint)),
        _ => Err(candidates.iter().map(|endpoint| endpoint.address).collect()),
    }
}
