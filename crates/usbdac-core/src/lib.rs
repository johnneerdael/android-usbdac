//! usbdac-core: bit-perfect direct USB DAC output (work in progress).

pub mod descriptors;

mod clock_resolution;
mod endpoint;
mod iso_packet_scheduler;
mod iso_packet_validation;
mod iso_transport_loss;
mod output_position;
mod payload_audit_latch;
mod quirk;
mod volume_control;

#[cfg(test)]
mod tests;
