//! The public surface, used the way a host uses it. Nothing here touches USB.

use std::sync::Arc;
use usbdac_core::{
    BitPerfectBlocker, Dac, DacConfig, DacEvent, DacListener, DacStream, DeviceInfo, Encoding, Error,
    HandBack,
    Quirk, QuirkOverride, RatePolicy, StreamFormat, StreamRequest, StreamState, VolumeControl,
};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn the_handles_can_be_shared_between_threads() {
    assert_send_sync::<Dac>();
    assert_send_sync::<DacStream>();
    assert_send_sync::<VolumeControl>();
    assert_send_sync::<StreamFormat>();
    assert_send_sync::<Error>();
    assert_send_sync::<DacEvent>();
    assert_send_sync::<Arc<dyn DacListener>>();
}

#[test]
fn a_closure_is_a_listener() {
    let listener: Arc<dyn DacListener> = Arc::new(|event: DacEvent| {
        let _ = event;
    });
    listener.on_event(DacEvent::Underrun);
}

#[test]
fn errors_have_stable_codes_and_a_readable_message() {
    let cases: Vec<(Error, &str)> = vec![
        (Error::Busy, "busy"),
        (Error::PlatformUnsupported, "platform_unsupported"),
        (Error::Claim("x".into()), "claim"),
        (
            Error::Unsupported {
                reason: "no".into(),
                offered: Vec::new(),
            },
            "unsupported",
        ),
        (
            Error::ClockUnverified {
                requested_hz: 96_000,
                reported_hz: Some(48_000),
            },
            "clock_unverified",
        ),
        (Error::ImplicitFeedbackUnsupported, "implicit_feedback_unsupported"),
        (Error::DeviceLost, "device_lost"),
        (Error::Transfer("x".into()), "transfer"),
        (Error::Control("x".into()), "control"),
        (Error::InvalidState("x".into()), "invalid_state"),
        (Error::InvalidArgument("x".into()), "invalid_argument"),
    ];
    for (error, code) in cases {
        assert_eq!(error.code(), code);
        assert!(error.to_string().starts_with(&format!("{code}: ")), "{error}");
    }
    let unverified = Error::ClockUnverified {
        requested_hz: 96_000,
        reported_hz: Some(48_000),
    };
    assert!(unverified.message().contains("96000") && unverified.message().contains("48000"));
}

#[test]
fn a_config_is_built_from_the_defaults_with_struct_update_syntax() {
    let config = DacConfig {
        buffer_target_ms: 150,
        hand_back: HandBack::Keep,
        dsd_big_endian: Some(true),
        quirks: vec![QuirkOverride {
            vendor_id: 0x1234,
            product_id: 0x5678,
            name_contains: None,
            quirks: vec![Quirk::SettleDelayMs(100), Quirk::DsdSubslotSize(4)],
        }],
        ..DacConfig::default()
    };
    assert!(config.validate().is_ok());
    assert_eq!(config.buffer_capacity_ms, 500);
    assert!(!config.allow_unsynchronized_async, "implicit feedback is refused by default");
}

#[test]
fn requests_describe_each_encoding() {
    let pcm = StreamRequest::pcm(96_000, 2, 24);
    assert_eq!(pcm.encoding, Encoding::Pcm);
    assert_eq!(pcm.rate_policy, RatePolicy::Exact);
    let dop = StreamRequest::dop(2_822_400, 2).with_rate_policy(RatePolicy::NearestSupported);
    assert_eq!(dop.sample_rate, 176_400);
    assert_eq!(dop.encoding, Encoding::DoP);
    assert_eq!(
        StreamRequest::dsd_native(5_644_800, 2).encoding,
        Encoding::DsdNative {
            bit_rate: 5_644_800
        }
    );
}

#[test]
fn stream_state_codes_are_the_documented_ones() {
    let codes: Vec<i32> = [
        StreamState::Buffering,
        StreamState::Playing,
        StreamState::Paused,
        StreamState::Draining,
        StreamState::Drained,
        StreamState::Closed,
        StreamState::Failed,
    ]
    .iter()
    .map(|state| state.code())
    .collect();
    assert_eq!(codes, vec![0, 1, 2, 3, 4, 5, 6]);
}

#[test]
fn opening_without_usbfs_fails_cleanly_and_leaves_the_slot_free() {
    let info = DeviceInfo::new(0x1234, 0x5678, "No such DAC").with_claimed_interfaces(vec![0u8, 1]);
    let first = Dac::open(-1, info.clone(), DacConfig::default()).unwrap_err();
    assert!(
        matches!(first, Error::PlatformUnsupported | Error::Claim(_)),
        "{first:?}"
    );
    // The failed attempt did not leave a registration behind: the next attempt
    // fails the same way instead of reporting Busy.
    let second = Dac::open(-1, info, DacConfig::default()).unwrap_err();
    assert_eq!(first, second);
}

#[test]
fn the_bit_perfect_blockers_have_stable_codes() {
    let codes: Vec<&str> = [
        BitPerfectBlocker::NoStream,
        BitPerfectBlocker::LostFrames,
        BitPerfectBlocker::FormatChanged,
        BitPerfectBlocker::ClockUnverified,
        BitPerfectBlocker::SoftwareGain,
        BitPerfectBlocker::UnsynchronizedAsync,
        BitPerfectBlocker::PayloadUnverified,
    ]
    .iter()
    .map(|blocker| blocker.code())
    .collect();
    assert_eq!(
        codes,
        [
            "no_stream",
            "lost_frames",
            "format_changed",
            "clock_unverified",
            "software_gain",
            "unsynchronized_async",
            "payload_unverified"
        ]
    );
}
