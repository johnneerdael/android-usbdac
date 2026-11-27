use std::sync::{Arc, Mutex};
use usbdac_core::{
    fake::FakeDac, DeviceInfo, Error, HotplugEvent, HotplugHub, PlaybackDevice, PlaybackStream,
    StreamRequest, StreamState, UsbPermission,
};

#[test]
fn reused_path_cannot_accept_a_stale_permission_result_or_detach() {
    let hub = HotplugHub::new();
    let events = hub.subscribe();
    assert_eq!(events.try_recv().unwrap(), HotplugEvent::Snapshot(vec![]));
    let first = hub.attached(
        "/dev/bus/usb/001/002",
        DeviceInfo::new(1, 2, "DAC"),
        UsbPermission::Required,
    );
    assert_eq!(
        hub.attached(
            first.host_key.clone(),
            first.info.clone(),
            UsbPermission::Required
        )
        .id,
        first.id
    );
    assert!(hub.permission_changed(first.id, UsbPermission::Denied));
    assert!(hub.detached(first.id));
    let replacement = hub.attached(
        first.host_key.clone(),
        first.info.clone(),
        UsbPermission::Required,
    );
    assert_ne!(first.id, replacement.id);
    assert!(!hub.permission_changed(first.id, UsbPermission::Granted));
    assert!(!hub.detached(first.id));
    assert_eq!(hub.snapshot(), vec![replacement]);
}

#[test]
fn authoritative_snapshot_removes_missed_detach_and_rejects_old_permission() {
    let hub = HotplugHub::new();
    let events = hub.subscribe();
    events.try_recv().unwrap();
    let old = hub.attached("old", DeviceInfo::new(1, 2, "old"), UsbPermission::Required);
    let kept = hub.attached(
        "kept",
        DeviceInfo::new(3, 4, "kept"),
        UsbPermission::Required,
    );
    events.try_recv().unwrap();
    events.try_recv().unwrap();
    hub.reconcile(vec![(
        "kept".into(),
        kept.info.clone(),
        UsbPermission::Granted,
    )]);
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Detached(old.clone())
    );
    let mut updated = kept.clone();
    updated.permission = UsbPermission::Granted;
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::PermissionChanged(updated.clone())
    );
    assert_eq!(hub.snapshot(), vec![updated]);
    assert!(!hub.permission_changed(old.id, UsbPermission::Granted));
    hub.reconcile(vec![(
        "old".into(),
        old.info.clone(),
        UsbPermission::Required,
    )]);
    assert_ne!(hub.snapshot()[0].id, old.id);
}

#[test]
fn snapshot_changed_usb_identity_invalidates_old_permission() {
    let hub = HotplugHub::new();
    let old = hub.attached(
        "port",
        DeviceInfo::new(1, 2, "old"),
        UsbPermission::Required,
    );
    let events = hub.subscribe();
    events.try_recv().unwrap();
    hub.reconcile(vec![(
        "port".into(),
        DeviceInfo::new(3, 4, "new"),
        UsbPermission::Required,
    )]);
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Detached(old.clone())
    );
    let replacement = hub.snapshot().pop().unwrap();
    assert_ne!(old.id, replacement.id);
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Attached(replacement)
    );
    assert!(!hub.permission_changed(old.id, UsbPermission::Granted));
}

#[test]
fn attach_event_with_changed_identity_replaces_before_announcing_new_device() {
    let hub = HotplugHub::new();
    let old = hub.attached(
        "port",
        DeviceInfo::new(1, 2, "old"),
        UsbPermission::Required,
    );
    let events = hub.subscribe();
    events.try_recv().unwrap();
    let new = hub.attached(
        "port",
        DeviceInfo::new(3, 4, "new"),
        UsbPermission::Required,
    );
    assert!(new.id > old.id);
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Detached(old.clone())
    );
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Attached(new.clone())
    );
    assert!(!hub.permission_changed(old.id, UsbPermission::Granted));
    assert!(!hub.detached(old.id));
    assert_eq!(
        hub.attached("port", new.info.clone(), UsbPermission::Granted),
        new
    );
    assert!(events.try_recv().is_err());
}

#[test]
fn subscriber_overflow_replaces_old_hints_with_atomic_snapshot() {
    let hub = HotplugHub::new();
    let events = hub.subscribe();
    for key in 0..100 {
        hub.attached(
            key.to_string(),
            DeviceInfo::new(1, 2, "DAC"),
            UsbPermission::Unknown,
        );
    }
    assert_eq!(
        events.try_recv().unwrap(),
        HotplugEvent::Snapshot(hub.snapshot())
    );
    assert!(events.try_recv().is_err());
    let next = hub.attached("new", DeviceInfo::new(3, 4, "DAC"), UsbPermission::Required);
    assert_eq!(events.try_recv().unwrap(), HotplugEvent::Attached(next));
}

fn enqueue<D: PlaybackDevice>(dac: &D, request: &StreamRequest) -> D::Stream {
    let format = dac.negotiate(request).unwrap();
    let stream = dac.open_stream(&format, Arc::new(|_| {})).unwrap();
    assert_eq!(stream.write(&[0; 64]).unwrap(), 64);
    stream
}
#[test]
fn application_boundary_covers_backpressure_pause_flush_drain_loss_and_unplug() {
    let request = StreamRequest::pcm(48_000, 2, 16);
    let dac = FakeDac::new(DeviceInfo::new(1, 2, "Fake"), vec![request], 16).unwrap();
    let stream = enqueue(&dac, &request);
    assert_eq!(stream.write(&[0; 4]).unwrap(), 0);
    assert!(matches!(
        dac.negotiate(&request),
        Err(Error::InvalidState(_))
    ));
    stream.pause().unwrap();
    assert_eq!(stream.advance(8).unwrap(), 0);
    stream.play().unwrap();
    assert_eq!(stream.advance(8).unwrap(), 8);
    assert_eq!(stream.lose_frames(2).unwrap(), 2);
    assert_eq!(stream.lost_frames(), 2);
    assert_eq!(stream.played_frames(), 8);
    stream.drain().unwrap();
    assert!(stream.write(&[0; 4]).is_err());
    assert_eq!(stream.advance(20).unwrap(), 6);
    assert_eq!(stream.state(), StreamState::Drained);
    stream.flush().unwrap();
    assert_eq!(stream.played_frames(), 0);
    assert_eq!(stream.write(&[0; 4]).unwrap(), 4);
    dac.detach();
    assert_eq!(stream.write(&[0; 4]), Err(Error::DeviceLost));
    assert_eq!(stream.writable_bytes(), 0);
}

#[test]
fn fake_callbacks_report_loss_and_detach_without_requiring_usb() {
    let request = StreamRequest::pcm(44_100, 2, 24);
    let dac = FakeDac::new(DeviceInfo::new(1, 2, "Fake"), vec![request], 2).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let output = seen.clone();
    let format = dac.negotiate(&request).unwrap();
    let stream = dac
        .open_stream(
            &format,
            Arc::new(move |event| output.lock().unwrap().push(event)),
        )
        .unwrap();
    assert!(stream.write(&[0; 5]).is_err());
    assert_eq!(stream.write(&[0; 12]).unwrap(), 12);
    stream.lose_frames(1).unwrap();
    dac.detach();
    assert!(seen
        .lock()
        .unwrap()
        .contains(&usbdac_core::DacEvent::LostFrames { frames: 1 }));
    assert!(seen
        .lock()
        .unwrap()
        .contains(&usbdac_core::DacEvent::DeviceLost));
}
