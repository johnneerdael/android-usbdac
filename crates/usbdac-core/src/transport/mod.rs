//! The usbfs/libusb transport: descriptor scan, clock programming, the
//! isochronous output loop with explicit-feedback timing, loss accounting and
//! the Feature Unit volume controls.
//!
//! Everything here is crate-internal. The public API in [`crate::dac`] and
//! [`crate::stream`] is a thin layer over it.
//!
//! v0.1 keeps one process-wide session (`session::SESSION_STATE` and friends):
//! one DAC at a time is the reality on a phone, and the claimed interfaces,
//! the libusb handle and the output thread are inherently single-owner. The
//! statics never leak into the public types.

#![allow(clippy::too_many_arguments, clippy::type_complexity)]

use crate::clock_resolution::{audio_control_descriptors_from_config, ClockGraph};
use crate::error::Error;
use crate::events::EventSink;
use crate::stream::BitPerfectBlocker;
use crate::descriptors::{
    AudioControlParser, DescriptorIter, FeatureUnit, InputTerminal, OutputTerminal,
};
use crate::endpoint::{select_explicit_feedback_endpoint, FeedbackEndpointCandidate};
use crate::iso_packet_scheduler::{IsoPacketScheduler, IsoQueueGeometry};
use crate::iso_packet_validation::{
    account_timed_out_transfer, iso_transfer_status_label, validate_iso_packet_descriptors,
    CompletionStats, PacketAcceptance,
};
use crate::iso_transport_loss::{
    lost_frame_budget, ring_bytes_per_frame, LossLimits, LossLogGate, LossTracker, UrbLoss,
};
use crate::output_position::{
    inflight_music_frames, music_frames_delivered, output_latency_frames, PlayedMusicCounter,
};
use crate::payload_audit_latch::PayloadAuditLatch;
use crate::settings::settings;
use crate::volume_control::{
    common_channel_volume, parse_uac2_volume_ranges, quantize_volume,
    read_hardware_volume_snapshot, volume_bounds, AudioControlProtocol, OriginalVolumeSlot,
    RawRange,
};
use libusb1_sys::{
    constants::{
        LIBUSB_TRANSFER_CANCELLED, LIBUSB_TRANSFER_COMPLETED, LIBUSB_TRANSFER_ERROR,
        LIBUSB_TRANSFER_NO_DEVICE, LIBUSB_TRANSFER_OVERFLOW, LIBUSB_TRANSFER_STALL,
        LIBUSB_TRANSFER_TIMED_OUT,
    },
    libusb_alloc_transfer, libusb_cancel_transfer, libusb_fill_interrupt_transfer,
    libusb_fill_iso_transfer, libusb_free_transfer, libusb_submit_transfer, libusb_transfer,
};
use log::{error as log_error, info as log_info, warn as log_warn};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusb::{
    disable_device_discovery, ConfigDescriptor, Context, Device, DeviceHandle, Direction, Speed,
    SyncType, TransferType, UsageType, UsbContext,
};
use std::collections::HashSet;
use std::ffi::c_void;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The ioctl request type: `libc::Ioctl` exists on Linux and Android only; the
/// other hosts compile the same code for type-checking and unit tests.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) type IoctlRequest = libc::Ioctl;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) type IoctlRequest = libc::c_ulong;

const USB_CLASS_AUDIO: u8 = 0x01;
const USB_SUBCLASS_AUDIOCONTROL: u8 = 0x01;
const USB_SUBCLASS_AUDIOSTREAMING: u8 = 0x02;
const USB_DT_CS_INTERFACE: u8 = 0x24;
const USB_DIR_IN: u8 = 0x80;
const USB_DIR_OUT: u8 = 0x00;
const USB_TYPE_CLASS: u8 = 0x20;
const USB_RECIP_INTERFACE: u8 = 0x01;
const USB_RECIP_ENDPOINT: u8 = 0x02;
const UAC2_AS_GENERAL: u8 = 0x01;
const UAC2_FORMAT_TYPE: u8 = 0x02;
const UAC2_FORMAT_TYPE_I: u8 = 0x01;
const UAC2_REQUEST_SET_CUR: u8 = 0x01;
const UAC2_REQUEST_GET_CUR: u8 = 0x01;
const UAC2_REQUEST_GET_RANGE: u8 = 0x02;
const UAC1_REQUEST_GET_CUR: u8 = 0x81;
const UAC2_CLOCK_SOURCE_SAM_FREQ_CONTROL: u16 = 0x0100;
const UAC2_CLOCK_SOURCE_CLOCK_VALID_CONTROL: u16 = 0x0200;
/// CX_CLOCK_SELECTOR_CONTROL: GET_CUR returns the 1-based input pin in use.
const UAC2_CLOCK_SELECTOR_CONTROL: u16 = 0x0100;
const UAC1_SAM_FREQ_CONTROL: u16 = 0x0100;
const UAC2_INPUT_TERMINAL: u8 = 0x02;
const UAC2_OUTPUT_TERMINAL: u8 = 0x03;
const UAC2_FEATURE_UNIT: u8 = 0x06;
const FORMAT_TAG_PCM: u16 = 0x0001;
const FORMAT_TAG_PCM8: u16 = 0x0002;
const FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;
const FORMAT_TAG_DSD: u16 = 0x0008;
const UAC1_REQUEST_GET_MIN: u8 = 0x82;
const UAC1_REQUEST_GET_MAX: u8 = 0x83;
const UAC1_REQUEST_GET_RES: u8 = 0x84;
const UAC2_FEATURE_UNIT_MUTE_CONTROL: u16 = 0x0100;
const UAC2_FEATURE_UNIT_VOLUME_CONTROL: u16 = 0x0200;
const ISO_TRANSFER_TIMEOUT_MS: u32 = 1000;
// A lossy data URB (it timed out, or the DAC took fewer bytes than were queued)
// cannot be re-sent: up to `transfers_in_flight` later URBs are already queued
// behind it and re-sending would play out of order. So loss is counted, clears
// the bit-perfect status at the first lost frame, and is bounded two ways so a
// broken transport stops the stream instead of playing glitched audio
// indefinitely.
//
// More than `IsoQueueGeometry::max_consecutive_lossy_urbs` lossy URBs in a row
// (three of the default 2 ms transfers, and never less than about 6 ms of
// audio whatever the transfer size) means the host or DAC has stopped
// consuming, not that one slot was unlucky.
//
// Cumulative budget since stream start, for loss that is sporadic and so never
// trips the consecutive bound. 100 ms is about 50 default URBs' worth of audio
// (a default transfer carries 2 ms at every service interval): dozens of audible dropouts, far
// more than a healthy link ever produces, yet short enough that a stream which
// keeps shedding frames is not left running for hours. The bit-perfect status
// is already gone after the first lost frame, so this only decides when to stop
// playing. Counted in ring frames, so it scales with `effective_ring_rate`.
const USB_MAX_LOST_AUDIO_MS: u64 = 100;
// Loss log lines: the first few immediately, then at most one per interval.
const USB_LOSS_LOG_BURST: u32 = 4;
const USB_LOSS_LOG_MIN_INTERVAL_MS: u64 = 1_000;
const USB_FEEDBACK_TIMEOUT_MS: u32 = 50;
/// Longest window one feedback transfer polls over; shortened to the bus queue
/// (`DacConfig::bus_queue_ms`) when that is shallower.
const USB_FEEDBACK_POLL_WINDOW_MS: u32 = 16;
const USB_FEEDBACK_STALE_WARN_MS: u64 = 500;
const USB_PAYLOAD_FLICKER_GRACE_MS: u64 = 1500;
const USB_TIMING_LOG_INTERVAL: u64 = 128;

pub(crate) mod api;
mod clock;
mod encode;
mod iso;
mod negotiate;
mod output;
mod ring;
mod scan;
pub(crate) mod session;
pub(crate) mod shared;
mod stream_open;
mod types;
mod usbfs;
pub(crate) mod volume_hw;
#[cfg(test)]
pub(crate) mod fake_dac;
#[cfg(test)]
mod tests;

use clock::*;
use encode::*;
use iso::*;
use negotiate::*;
use output::*;
use ring::*;
use scan::*;
use session::*;
use shared::StreamShared;
pub(crate) use stream_open::StreamHandle;
use stream_open::*;
use types::*;
use usbfs::*;
use volume_hw::*;
