//! Focused Linux usbfs lifecycle probe for the synthetic USB/IP audio fixture.
//! Usage: usbip_lifecycle /dev/bus/usb/BBB/DDD normal|bad-cur|stale-cur|badd|full-uac3|duplex|retire
#![cfg_attr(not(target_os = "linux"), allow(unused))]

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs::OpenOptions,
        os::fd::AsRawFd,
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };
    use usbdac_core::{
        Dac, DacConfig, DacEvent, DeviceInfo, Error, LinkSpeed, RatePolicy, StreamRequest,
        SyncKind, UacVersion,
    };

    const VID: u16 = 0x1209;
    const PID: u16 = 0xdaca;
    #[repr(C)]
    struct Control {
        kind: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout: u32,
        data: *mut u8,
    }

    fn control(
        fd: i32,
        kind: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> std::io::Result<usize> {
        let mut c = Control {
            kind,
            request,
            value,
            index,
            length: data.len() as u16,
            timeout: 1000,
            data: data.as_mut_ptr(),
        };
        let result = unsafe { libc::ioctl(fd, 0xc0185500u32 as _, &mut c) };
        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }
    fn alt(fd: i32) -> Result<u8, Box<dyn std::error::Error>> {
        let mut value = [0xff];
        assert_eq!(control(fd, 0x81, 10, 0, 1, &mut value)?, 1);
        Ok(value[0])
    }
    fn claim(fd: i32, interface: u32, acquire: bool) -> std::io::Result<()> {
        let command = if acquire {
            0x8004550fu32
        } else {
            0x80045510u32
        };
        if unsafe { libc::ioctl(fd, command as _, &interface) } != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn power(fd: i32) -> Result<u8, Box<dyn std::error::Error>> {
        let mut state = [0xff];
        assert_eq!(control(fd, 0xa1, 1, 0x0200, 0x0a00, &mut state)?, 1);
        Ok(state[0])
    }

    fn badd(
        dac: &Dac,
        fd: i32,
        caps: &usbdac_core::Capabilities,
    ) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(caps.uac_version, UacVersion::V3);
        assert_eq!(caps.link_speed, LinkSpeed::High);
        assert_eq!(caps.pcm_sample_rates, [48_000]);
        assert!(matches!(
            dac.negotiate(&StreamRequest::pcm(96_000, 2, 16)),
            Err(Error::Unsupported { .. })
        ));
        let format = dac.negotiate(
            &StreamRequest::pcm(44_100, 2, 16).with_rate_policy(RatePolicy::NearestSupported),
        )?;
        assert_eq!(
            (
                format.sample_rate,
                format.channels,
                format.valid_bits,
                format.subslot_bytes,
                format.interface_number,
                format.alt_setting,
                format.endpoint_address
            ),
            (48_000, 2, 16, 2, 1, 1, 1)
        );
        assert!(
            format.explicit_feedback,
            "BADD profile must expose feedback"
        );
        println!("BADD_FORMAT {format:?}");
        let stream = dac.open_stream(
            &format,
            Arc::new(|event: DacEvent| eprintln!("EVENT {event:?}")),
        )?;
        assert_eq!(alt(fd)?, 1);
        let mut pcm = Vec::with_capacity(4800 * 4);
        for i in 0..4800i32 {
            let v = (((i % 96) - 48) * 20) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&(-v).to_le_bytes());
        }
        for _ in 0..3 {
            stream.write(&pcm)?;
        }
        stream.play()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3)
            && (stream.played_frames() == 0
                || stream.stats().completed_transfers == 0
                || stream.stats().feedback_reports == 0)
        {
            let n = stream.writable_bytes().min(pcm.len());
            if n >= 4 {
                stream.write(&pcm[..n - n % 4])?;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let stats = stream.stats();
        println!("BADD_PCM_STATS {stats:?}");
        assert!(
            stats.played_frames > 0 && stats.completed_transfers > 0 && stats.feedback_reports > 0,
            "BADD playback or feedback has no real completions"
        );
        stream.close()?;
        Ok(())
    }

    fn full_uac3(
        dac: Dac,
        fd: i32,
        caps: &usbdac_core::Capabilities,
        volatile: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use usbdac_core::routing::{
            ControlKey, Entity, RouteEdit, RouteValue, RoutingHandBackOutcome,
        };
        assert_eq!(caps.uac_version, UacVersion::V3);
        assert_eq!(caps.link_speed, LinkSpeed::High);
        let key = ControlKey::Selector(Entity {
            interface: 0,
            id: 8,
        });
        assert_eq!(power(fd)?, 2, "initial power must be D2");
        let snapshot = dac.routing_discover_powered()?;
        println!("UAC3_ROUTING snapshot={snapshot:?}");
        assert_eq!(snapshot.configuration(), 3);
        assert_eq!(power(fd)?, 2, "discovery must restore D2");
        assert_eq!(
            dac.routing_read_powered(&snapshot, key)?,
            RouteValue::SelectorPin(1)
        );
        assert_eq!(power(fd)?, 2, "read must restore D2");
        let report = dac.routing_apply_powered(
            &snapshot,
            &[RouteEdit {
                key,
                expected: RouteValue::SelectorPin(1),
                value: RouteValue::SelectorPin(2),
            }],
        )?;
        assert_eq!(report.changed, vec![key]);
        let applied = out_of_band("snapshot")?;
        assert!(
            applied.contains("selector=[2]"),
            "fixture selector was not written: {applied}"
        );
        assert!(
            applied.contains(if volatile {
                "selector_state=1"
            } else {
                "selector_state=2"
            }),
            "unexpected fixture selector after D2: {applied}"
        );
        assert_eq!(power(fd)?, 2, "apply must restore D2");
        let updated = dac.routing_discover_powered()?;
        assert_eq!(
            dac.routing_read_powered(&updated, key)?,
            RouteValue::SelectorPin(if volatile { 1 } else { 2 }),
            "selector readback must reflect the fixture's D2 behavior"
        );
        assert_eq!(power(fd)?, 2);
        dac.close()?;
        assert!(Dac::await_usb_retirement(Duration::from_secs(3)));
        let hand_back = snapshot
            .hand_back_report()
            .ok_or("missing routing hand-back report")?;
        println!("UAC3_HAND_BACK {hand_back:?}");
        if !volatile {
            assert!(
                hand_back
                    .controls
                    .contains(&(key, RoutingHandBackOutcome::Restored)),
                "selector hand-back was not verified"
            );
        }
        assert_eq!(power(fd)?, 2, "final power must be D2");
        assert!(
            out_of_band("snapshot")?.contains("selector_state=1"),
            "fixture selector was not restored"
        );
        assert_eq!(alt(fd)?, 0);
        for interface in [1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode=full-uac3 selector=restored power=D2 retired=true");
        Ok(())
    }

    // Full UAC3 whose clock RANGE lists only 48 kHz behind a powered route that
    // cannot certify a live refusal. The cached model excludes 44.1 kHz, so the
    // ranked loop and the playback candidates must skip it without any USB request,
    // as ALSA never offers a rate absent from its format table (pcm.c hw_rule_rate).
    fn full_uac3_nearest(
        dac: Dac,
        fd: i32,
        caps: &usbdac_core::Capabilities,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use usbdac_core::playback::PlaybackPolicy;
        assert_eq!(caps.uac_version, UacVersion::V3);
        assert_eq!(caps.link_speed, LinkSpeed::High);
        let mut rates = caps.pcm_sample_rates.clone();
        rates.sort_unstable();
        rates.dedup();
        assert_eq!(rates, [48_000], "unexpected full-UAC3 fixture rates");
        let no_44k = |snapshot: &str| -> Result<(), Box<dyn std::error::Error>> {
            let sets = snapshot.split(';').next().ok_or("missing rate snapshot")?;
            if sets
                .split(|c: char| !c.is_ascii_digit())
                .any(|v| v == "44100")
            {
                return Err(format!("model-excluded 44100 Hz was programmed: {snapshot}").into());
            }
            Ok(())
        };
        out_of_band("reset")?;
        let format = dac.negotiate(
            &StreamRequest::pcm(44_100, 2, 16).with_rate_policy(RatePolicy::NearestSupported),
        )?;
        assert_eq!(
            (format.sample_rate, format.channels, format.valid_bits),
            (48_000, 2, 16)
        );
        let negotiated = out_of_band("snapshot")?;
        no_44k(&negotiated)?;
        out_of_band("reset")?;
        let decision = dac.resolve_playback(
            &StreamRequest::pcm(44_100, 2, 16),
            &PlaybackPolicy::default(),
        )?;
        assert_eq!(decision.format.sample_rate, 48_000);
        assert_eq!(decision.candidates[0], StreamRequest::pcm(44_100, 2, 16));
        assert!(
            matches!(decision.exact_refusal, Some(Error::Unsupported { .. })),
            "the skipped exact source must report a typed refusal: {:?}",
            decision.exact_refusal
        );
        let resolved = out_of_band("snapshot")?;
        no_44k(&resolved)?;
        println!(
            "NEGOTIATE_CASE mode=full-uac3-nearest requested=44100 actual=48000 resolve=48000 attempted={} negotiate={negotiated} resolve_snapshot={resolved}",
            decision.attempted_candidates
        );
        assert_eq!(alt(fd)?, 0);
        dac.close()?;
        assert!(Dac::await_usb_retirement(Duration::from_secs(3)));
        assert_eq!(alt(fd)?, 0);
        for interface in [1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode=full-uac3-nearest retired=true");
        Ok(())
    }

    fn vendor(fd: i32, value: u16) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(control(fd, 0x40, 0x5a, value, 0, &mut [])?, 0);
        Ok(())
    }
    fn vendor_status(fd: i32) -> Result<u8, Box<dyn std::error::Error>> {
        let mut status = [0u8];
        assert_eq!(control(fd, 0xc0, 0x5a, 0, 0, &mut status)?, 1);
        Ok(status[0])
    }
    fn out_of_band(action: &str) -> Result<String, Box<dyn std::error::Error>> {
        let port = std::env::var("USBDAC_USBIP_PORT")?;
        let _: u16 = port.parse()?;
        let output = std::process::Command::new("/fixture/usbip_uac")
            .args(["--control", action, "--port", &port])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "fixture control {action} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }
    fn wait_status(fd: i32, mask: u8) -> Result<u8, Box<dyn std::error::Error>> {
        let start = Instant::now();
        loop {
            let status = vendor_status(fd)?;
            if status & mask == mask {
                return Ok(status);
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return Err(
                    format!("fixture status {status:#04x} did not reach {mask:#04x}").into(),
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn test_pcm() -> Vec<u8> {
        let mut pcm = Vec::with_capacity(4800 * 4);
        for i in 0..4800i32 {
            let v = (((i % 96) - 48) * 20) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&(-v).to_le_bytes());
        }
        pcm
    }
    fn feed(stream: &usbdac_core::DacStream, pcm: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let n = stream.writable_bytes().min(pcm.len());
        if n >= 4 {
            stream.write(&pcm[..n - n % 4])?;
        }
        Ok(())
    }
    fn duplex(dac: Dac, fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        use usbdac_core::{CaptureRequest, CaptureState};
        let format = dac.negotiate(&StreamRequest::pcm(48_000, 2, 16))?;
        let request = CaptureRequest {
            sample_rate: 48_000,
            channels: 2,
            valid_bits: 16,
            channel_map: None,
        };
        let (playback, capture) = dac.open_duplex(
            &format,
            &request,
            48_000,
            Arc::new(|e: DacEvent| eprintln!("EVENT {e:?}")),
        )?;
        assert_eq!(
            (capture.interface_number(), capture.endpoint_address()),
            (3, 0x82)
        );
        let pcm = test_pcm();
        for _ in 0..3 {
            playback.write(&pcm)?;
        }
        playback.play()?;
        let start = Instant::now();
        let mut received = [0u8; 4096];
        let mut marker_frames = 0;
        while start.elapsed() < Duration::from_secs(5) && marker_frames == 0 {
            feed(&playback, &pcm)?;
            let frames = capture.read(&mut received)?;
            if frames > 0 {
                assert!(
                    received[..frames * 4]
                        .chunks_exact(4)
                        .all(|frame| frame == [0x34, 0x12, 0xcc, 0xed]),
                    "unexpected capture payload"
                );
                marker_frames = frames;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(marker_frames > 0, "no actual capture marker");
        println!("DUPLEX_CAPTURE_MARKER frames={marker_frames}");
        out_of_band("mixed")?;
        let mixed_start = Instant::now();
        while mixed_start.elapsed() < Duration::from_secs(5) {
            feed(&playback, &pcm)?;
            let frames = capture.read(&mut received)?;
            assert!(
                received[..frames * 4]
                    .chunks_exact(4)
                    .all(|frame| frame == [0x34, 0x12, 0xcc, 0xed]),
                "invalid packet leaked into capture"
            );
            let stats = capture.stats();
            if stats.short_packets > 0 && stats.failed_packets > 0 && stats.malformed_packets > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mixed_stats = capture.stats();
        assert!(
            mixed_stats.short_packets > 0
                && mixed_stats.failed_packets > 0
                && mixed_stats.malformed_packets > 0,
            "mixed packet statuses not observed: {mixed_stats:?}"
        );
        println!("DUPLEX_MIXED_PACKETS {mixed_stats:?}");
        vendor(fd, 1)?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && capture.state() == CaptureState::Active {
            feed(&playback, &pcm)?;
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(capture.state(), CaptureState::Failed);
        assert!(
            capture.last_error().is_some(),
            "capture failure must preserve an error"
        );
        assert_ne!(
            vendor_status(fd)? & 8,
            0,
            "fixture did not deliver capture fault"
        );
        let before = playback.played_frames();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && playback.played_frames() <= before {
            feed(&playback, &pcm)?;
            thread::sleep(Duration::from_millis(5));
        }
        let after = playback.played_frames();
        assert!(
            after > before,
            "playback stopped after independent capture failure"
        );
        println!(
            "DUPLEX_ISOLATION_PASS before={before} after={after} error={:?}",
            capture.last_error()
        );
        capture.close()?;
        playback.close()?;
        let retirement = dac.retirement();
        dac.close()?;
        assert!(retirement.wait(Duration::from_secs(3)));
        assert_eq!(alt(fd)?, 0);
        for interface in [3, 2, 1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode=duplex retired=true");
        Ok(())
    }
    fn retire(dac: Dac, fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        let format = dac.negotiate(&StreamRequest::pcm(48_000, 2, 16))?;
        let playback =
            dac.open_stream(&format, Arc::new(|e: DacEvent| eprintln!("EVENT {e:?}")))?;
        let pcm = test_pcm();
        for _ in 0..3 {
            playback.write(&pcm)?;
        }
        playback.play()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && playback.played_frames() == 0 {
            feed(&playback, &pcm)?;
            thread::sleep(Duration::from_millis(5));
        }
        assert!(playback.played_frames() > 0, "playback never completed");
        let token = dac.retirement();
        vendor(fd, 1)?;
        let status = wait_status(fd, 2)?;
        println!("RETIRE_HELD status={status:#04x}");
        let start = Instant::now();
        let result = dac.close();
        let elapsed = start.elapsed();
        println!(
            "RETIRE_CLOSE result={result:?} elapsed_ms={}",
            elapsed.as_millis()
        );
        assert!(
            matches!(result, Err(Error::Transfer(_))),
            "close must report timeout: {result:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(2500),
            "close returned before its actual timeout: {elapsed:?}"
        );
        assert!(
            !token.wait(Duration::ZERO),
            "owner retired while URB unlink was held"
        );
        let status: u8 = out_of_band("status")?.parse()?;
        assert_eq!(
            status & 0x14,
            0x14,
            "unlink must be held and AS1 active: {status:#04x}"
        );
        println!("RETIRE_PENDING elapsed_ms={}", elapsed.as_millis());
        out_of_band("release")?;
        assert!(
            token.wait(Duration::from_secs(10)),
            "owner did not retire after unlink release"
        );
        assert_eq!(alt(fd)?, 0, "retired owner did not restore alt zero");
        drop(playback);
        for interface in [1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode=retire retired=true alt=0");
        Ok(())
    }

    fn reconfigure(dac: Dac, fd: i32) -> Result<(), Box<dyn std::error::Error>> {
        use usbdac_core::StreamState;
        let listener = Arc::new(|e: DacEvent| eprintln!("EVENT {e:?}"));
        let initial = dac.negotiate(&StreamRequest::pcm(96_000, 2, 16))?;
        assert_eq!(
            (
                initial.sample_rate,
                initial.interface_number,
                initial.alt_setting
            ),
            (96_000, 1, 1)
        );
        let previous = dac.open_stream(&initial, listener.clone())?;
        let pcm = test_pcm();
        for _ in 0..3 {
            previous.write(&pcm)?;
        }
        previous.play()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) && previous.played_frames() == 0 {
            feed(&previous, &pcm)?;
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            previous.played_frames() > 0,
            "original playback never completed"
        );
        out_of_band("reset")?;
        println!("RECONFIGURE_PHASE_BEGIN from=96000 to=48000");
        let request = StreamRequest::pcm(48_000, 2, 16);
        let replacement = dac.reconfigure_stream_candidates(
            &previous,
            &[request],
            listener,
            Duration::from_secs(3),
        )?;
        println!("RECONFIGURE_PHASE_END from=96000 to=48000");
        assert!(
            replacement.rollback_cause.is_none(),
            "unexpected rollback: {:?}",
            replacement.rollback_cause
        );
        assert_eq!(previous.state(), StreamState::Closed);
        assert_eq!(replacement.stream.state(), StreamState::Paused);
        assert_eq!(replacement.stream.format().sample_rate, 48_000);
        assert_eq!(replacement.stream.played_frames(), 0);
        assert_eq!(alt(fd)?, 1);
        let transitions = out_of_band("snapshot")?;
        let alt_history = transitions
            .split("alts=[")
            .nth(1)
            .ok_or("missing alternate history")?
            .split(']')
            .next()
            .ok_or("missing alternate terminator")?;
        let unlink_count: u64 = transitions
            .split("unlinks=")
            .nth(1)
            .ok_or("missing unlink count")?
            .parse()?;
        assert!(
            unlink_count <= 16,
            "more than one bounded transfer set cancelled: {transitions}"
        );
        let mut alts: Vec<_> = alt_history.split(", ").collect();
        alts.dedup();
        assert_eq!(
            alts,
            ["0", "1"],
            "unexpected alternate transitions: {transitions}"
        );
        // USB-level count of the live 96 -> 48 kHz replacement on interface 1 and
        // clock 16. ALSA's UAC2 prepare deselects the alternate first, writes the
        // rate, and enables the alternate last (sound/usb/endpoint.c
        // snd_usb_endpoint_prepare:1498-1530, endpoint_set_interface:917-951).
        // ALSA elides a SET_INTERFACE to the alternate it already holds
        // (endpoint.c:926-927); usbdac re-asserts alt 0 at stop, before and after
        // the read-only negotiation and before activation, which is idempotent.
        let sequence = out_of_band("sequence")?;
        let writes: Vec<_> = sequence
            .strip_prefix("seq=[")
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or("missing write sequence")?
            .split(", ")
            .collect();
        let count = |w: &str| writes.iter().filter(|x| **x == w).count();
        assert_eq!(
            (
                count("alt0"),
                count("alt1"),
                count("rate48000"),
                writes.len()
            ),
            (4, 1, 2, 7),
            "unexpected replacement write counts: {sequence}"
        );
        assert_eq!(
            writes.first(),
            Some(&"alt0"),
            "old alternate not deselected first: {sequence}"
        );
        assert_eq!(
            writes.last(),
            Some(&"alt1"),
            "alternate not enabled last: {sequence}"
        );
        assert!(
            !writes
                .iter()
                .any(|w| w.starts_with("rate") && *w != "rate48000"),
            "a rate other than 48 kHz was written: {sequence}"
        );
        // Fresh post-replacement evidence: the DAC's clock reads back 48 kHz at alt 1.
        let mut current = [0u8; 4];
        assert_eq!(control(fd, 0xa1, 1, 0x0100, 0x1000, &mut current)?, 4);
        assert_eq!(u32::from_le_bytes(current), 48_000);
        println!(
            "RECONFIGURE_USB_WRITES {sequence} clock_readback=48000 alt={}",
            alt(fd)?
        );
        let stream = replacement.stream;
        for _ in 0..3 {
            stream.write(&pcm)?;
        }
        stream.play()?;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5)
            && (stream.played_frames() == 0 || stream.stats().completed_transfers == 0)
        {
            feed(&stream, &pcm)?;
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            stream.played_frames() > 0 && stream.stats().completed_transfers > 0,
            "replacement playback never completed"
        );
        let stats = stream.stats();
        assert!(stats.played_frames <= stats.frames_written);
        assert!(stats.queued_frames <= stats.frames_written);
        assert!(
            stats.underruns <= 50,
            "USB/IP replacement exceeded 50 underruns: {stats:?}"
        );
        let mut current = [0u8; 4];
        assert_eq!(control(fd, 0xa1, 1, 0x0100, 0x1000, &mut current)?, 4);
        assert_eq!(
            u32::from_le_bytes(current),
            48_000,
            "clock drifted during replacement playback"
        );
        let fixture_stats = out_of_band("snapshot")?;
        assert!(
            !fixture_stats.contains("out_completions=0"),
            "replacement has no completed OUT URB: {fixture_stats}"
        );
        println!(
            "RECONFIGURE_PLAYBACK_PASS stats={stats:?} underrun_limit=50 fixture={fixture_stats}"
        );
        stream.close()?;
        let retirement = dac.retirement();
        dac.close()?;
        assert!(retirement.wait(Duration::from_secs(3)));
        assert_eq!(alt(fd)?, 0);
        for interface in [1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode=reconfigure retired=true alt=0");
        Ok(())
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args().collect();
        if args.len() != 3
            || !matches!(
                args[2].as_str(),
                "normal"
                    | "bad-cur"
                    | "stale-cur"
                    | "badd"
                    | "full-uac3"
                    | "full-uac3-volatile"
                    | "full-uac3-clock"
                    | "full-uac3-clock-bad"
                    | "full-uac3-clock-stale"
                    | "duplex"
                    | "retire"
                    | "reconfigure"
                    | "selector"
                    | "refuse-set"
                    | "zero-res"
                    | "full-uac3-nearest"
            )
        {
            return Err(
                "usage: usbip_lifecycle /dev/bus/usb/BBB/DDD normal|bad-cur|stale-cur|badd|full-uac3|full-uac3-clock|duplex|retire|reconfigure".into(),
            );
        }
        let path = &args[1];
        if !path.starts_with("/dev/bus/usb/") {
            return Err("expected a usbfs device path".into());
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let fd = file.as_raw_fd();
        // Read and validate the actual descriptor before claiming or invoking the driver.
        let mut descriptor = [0u8; 18];
        assert_eq!(control(fd, 0x80, 6, 0x0100, 0, &mut descriptor)?, 18);
        assert_eq!((descriptor[0], descriptor[1]), (18, 1));
        assert_eq!(
            (
                u16::from_le_bytes([descriptor[8], descriptor[9]]),
                u16::from_le_bytes([descriptor[10], descriptor[11]])
            ),
            (VID, PID),
            "refusing a device other than the fixture"
        );
        // Select the explicitly advertised fixture configuration before interface claims.
        let configuration = match args[2].as_str() {
            "badd" => 2u32,
            "full-uac3"
            | "full-uac3-volatile"
            | "full-uac3-clock"
            | "full-uac3-clock-bad"
            | "full-uac3-clock-stale"
            | "full-uac3-nearest" => 3,
            _ => 1,
        };
        if configuration != 1 {
            let mut config_descriptor = [0u8; 9];
            assert_eq!(
                control(
                    fd,
                    0x80,
                    6,
                    0x0200 | (configuration as u16 - 1),
                    0,
                    &mut config_descriptor
                )?,
                9
            );
            assert_eq!(
                (
                    config_descriptor[0],
                    config_descriptor[1],
                    config_descriptor[5]
                ),
                (9, 2, configuration as u8)
            );
            if unsafe { libc::ioctl(fd, 0x80045505u32 as _, &configuration) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        let claims = if args[2] == "duplex" {
            vec![0u8, 1, 2, 3]
        } else {
            vec![0u8, 1]
        };
        for &interface in &claims {
            claim(fd, u32::from(interface), true)?;
        }
        assert_eq!(alt(fd)?, 0);
        if matches!(
            args[2].as_str(),
            "badd" | "full-uac3" | "full-uac3-volatile"
        ) {
            assert_eq!(power(fd)?, 0, "power domain must initially be D0");
            let mut state = [2u8];
            assert_eq!(control(fd, 0x21, 1, 0x0200, 0x0a00, &mut state)?, 1);
            assert_eq!(power(fd)?, 2, "failed to seed D2 baseline");
        }
        let mut info = DeviceInfo::new(VID, PID, "USB/IP audio fixture")
            .with_claimed_interfaces(claims)
            .with_device_name(path);
        info.configuration_id = Some(configuration as u8);
        let config = DacConfig {
            require_hardware_volume: false,
            require_verified_rate: true,
            ..DacConfig::default()
        };
        let dac = if args[2] == "duplex" {
            Dac::open_duplex_device(fd, info, config)?
        } else {
            Dac::open(fd, info, config)?
        };
        if args[2] == "duplex" {
            return duplex(dac, fd);
        }
        if args[2] == "retire" {
            return retire(dac, fd);
        }
        if args[2] == "reconfigure" {
            return reconfigure(dac, fd);
        }
        let caps = dac.capabilities()?;
        if args[2] == "refuse-set" {
            out_of_band("reset")?;
            let requests = [
                StreamRequest::pcm(96_000, 2, 16),
                StreamRequest::pcm(48_000, 2, 16),
            ];
            let failure = dac.negotiate_candidates(&requests);
            assert!(
                matches!(
                    failure,
                    Err(Error::ClockUnverified {
                        requested_hz: 96_000,
                        ..
                    })
                ),
                "refused SET must be terminal: {failure:?}"
            );
            let snapshot = out_of_band("snapshot")?;
            let rates = snapshot.split(';').next().ok_or("missing rate snapshot")?;
            assert!(rates.contains("96000"));
            assert!(
                !rates.contains("48000"),
                "second candidate was tried after uncertain SET: {snapshot}"
            );
            let retirement = dac.retirement();
            dac.close()?;
            assert!(retirement.wait(Duration::from_secs(3)));
            assert_eq!(alt(fd)?, 0);
            for interface in [1, 0] {
                claim(fd, interface, false)?;
            }
            println!("USBIP_LIFECYCLE_PASS mode=refuse-set snapshot={snapshot}");
            return Ok(());
        }
        if matches!(args[2].as_str(), "selector" | "zero-res") {
            let (requested, expected) = if args[2] == "selector" {
                (192_000, 96_000)
            } else {
                (96_000, 44_100)
            };
            out_of_band("reset")?;
            let request =
                StreamRequest::pcm(requested, 2, 16).with_rate_policy(RatePolicy::NearestSupported);
            let format = dac.negotiate(&request)?;
            assert_eq!(format.sample_rate, expected);
            let stream = dac.open_stream(&format, Arc::new(|_| {}))?;
            let snapshot = out_of_band("snapshot")?;
            let rates = snapshot.split(';').next().ok_or("missing rate snapshot")?;
            let rejected = requested.to_string();
            assert!(
                !rates
                    .split(|c: char| !c.is_ascii_digit())
                    .any(|v| v == rejected),
                "requested unsupported rate was sent: {snapshot}"
            );
            println!(
                "NEGOTIATE_CASE mode={} requested={requested} actual={expected} {snapshot}",
                args[2]
            );
            stream.close()?;
            let retirement = dac.retirement();
            dac.close()?;
            assert!(retirement.wait(Duration::from_secs(3)));
            assert_eq!(alt(fd)?, 0);
            for interface in [1, 0] {
                claim(fd, interface, false)?;
            }
            println!("USBIP_LIFECYCLE_PASS mode={} retired=true", args[2]);
            return Ok(());
        }
        if args[2] == "badd" {
            badd(&dac, fd, &caps)?;
            assert_eq!(alt(fd)?, 0);
            dac.close()?;
            assert!(Dac::await_usb_retirement(Duration::from_secs(3)));
            assert_eq!(alt(fd)?, 0);
            assert_eq!(power(fd)?, 2, "BADD must restore D2");
            let power_log = out_of_band("snapshot")?;
            let powers = power_log
                .split("power=[")
                .nth(1)
                .ok_or("missing power history")?
                .split(']')
                .next()
                .ok_or("missing power terminator")?;
            assert!(
                powers
                    .split(", ")
                    .collect::<Vec<_>>()
                    .windows(2)
                    .any(|pair| pair == ["0", "2"]),
                "expected D0 to D2 transition: {power_log}"
            );
            for interface in [1, 0] {
                claim(fd, interface, false)?;
            }
            println!("USBIP_LIFECYCLE_PASS mode=badd alt=0 retired=true");
            return Ok(());
        }
        if args[2] == "full-uac3-nearest" {
            return full_uac3_nearest(dac, fd, &caps);
        }
        if matches!(args[2].as_str(), "full-uac3" | "full-uac3-volatile") {
            return full_uac3(dac, fd, &caps, args[2] == "full-uac3-volatile");
        }
        assert_eq!(
            caps.uac_version,
            if args[2].starts_with("full-uac3-clock") {
                UacVersion::V3
            } else {
                UacVersion::V2
            }
        );
        assert_eq!(caps.link_speed, LinkSpeed::High);
        let mut rates = caps.pcm_sample_rates.clone();
        rates.sort_unstable();
        rates.dedup();
        assert_eq!(rates, [48_000, 96_000], "unexpected fixture rates");
        assert!(
            matches!(
                dac.negotiate(&StreamRequest::pcm(192_000, 2, 16)),
                Err(Error::Unsupported { .. })
            ),
            "exact 192 kHz must be refused"
        );
        let request =
            StreamRequest::pcm(192_000, 2, 16).with_rate_policy(RatePolicy::NearestSupported);
        let format = dac.negotiate(&request)?;
        assert_eq!(
            (
                format.sample_rate,
                format.channels,
                format.valid_bits,
                format.subslot_bytes,
                format.interface_number,
                format.alt_setting
            ),
            (96_000, 2, 16, 2, 1, 1)
        );
        assert_eq!(format.sync_type, SyncKind::Synchronous);
        assert_eq!(format.endpoint_address, 0x01);
        assert_eq!(format.link_speed, LinkSpeed::High);
        assert_eq!(format.bytes_per_frame(), 4);
        if args[2].starts_with("full-uac3-clock") {
            assert_eq!(format.service_interval_us, 125);
            assert!(!format.explicit_feedback);
        }
        let clock = format
            .clock
            .as_ref()
            .ok_or("fixture clock missing")?
            .clone();
        assert_eq!(
            (clock.control_interface, clock.source_id),
            (
                0,
                if args[2].starts_with("full-uac3-clock") {
                    9
                } else {
                    0x10
                }
            )
        );
        println!("NEGOTIATED exact192=unsupported nearest192=96000 format={format:?}");
        let listener = Arc::new(|event: DacEvent| eprintln!("EVENT {event:?}"));
        match args[2].as_str() {
            "normal" | "full-uac3-clock" => {
                let stream = dac.open_stream(&format, listener)?;
                assert_eq!(alt(fd)?, 1);
                let mut current = [0u8; 4];
                assert_eq!(
                    control(
                        fd,
                        0xa1,
                        1,
                        0x0100,
                        (u16::from(clock.source_id) << 8) | u16::from(clock.control_interface),
                        &mut current
                    )?,
                    4
                );
                assert_eq!(
                    u32::from_le_bytes(current),
                    96_000,
                    "post-alt clock readback"
                );
                let mut pcm = Vec::with_capacity(9600 * 4);
                for i in 0..9600i32 {
                    let v = (((i % 96) - 48) * 20) as i16;
                    pcm.extend_from_slice(&v.to_le_bytes());
                    pcm.extend_from_slice(&(-v).to_le_bytes());
                }
                for _ in 0..3 {
                    stream.write(&pcm)?;
                }
                stream.play()?;
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(3)
                    && (stream.played_frames() == 0 || stream.stats().completed_transfers == 0)
                {
                    let n = stream.writable_bytes().min(pcm.len());
                    if n >= 4 {
                        stream.write(&pcm[..n - n % 4])?;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                let stats = stream.stats();
                println!("PCM_STATS {stats:?}");
                assert!(
                    stats.played_frames > 0 && stats.completed_transfers > 0,
                    "no actual PCM completions"
                );
                stream.close()?;
            }
            mode => match dac.open_stream(&format, listener) {
                Err(error @ Error::ClockUnverified { .. }) => {
                    let expected_reported = if mode == "bad-cur" || mode == "full-uac3-clock-bad" {
                        None
                    } else {
                        Some(48_000)
                    };
                    assert!(
                        matches!(error, Error::ClockUnverified { requested_hz: 96_000, reported_hz } if reported_hz == expected_reported)
                    );
                    println!(
                        "EXPECTED_FAILURE mode={mode} code={} error={error:?}",
                        error.code()
                    );
                }
                Err(error) => {
                    return Err(format!("unexpected failure for {mode}: {error:?}").into())
                }
                Ok(stream) => {
                    stream.close()?;
                    return Err(format!("{mode} incorrectly admitted the stream").into());
                }
            },
        }
        assert_eq!(alt(fd)?, 0, "stream teardown must restore alternate zero");
        dac.close()?;
        assert!(
            Dac::await_usb_retirement(Duration::from_secs(3)),
            "USB retirement timed out"
        );
        assert_eq!(alt(fd)?, 0);
        for interface in [1, 0] {
            claim(fd, interface, false)?;
        }
        println!("USBIP_LIFECYCLE_PASS mode={} alt=0 retired=true", args[2]);
        Ok(())
    }
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("usbip_lifecycle requires Linux usbfs");
}
