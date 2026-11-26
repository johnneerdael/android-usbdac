//! Events and their delivery.
//!
//! Every [`DacListener::on_event`] call comes from one library-owned thread
//! (`usbdac-events`), in order, and never while the library holds an internal
//! lock. The audio paths hand events to that thread through a bounded queue
//! with a non-blocking send, so a slow listener cannot stall the USB loop: if
//! the queue overflows, new events are dropped (and logged).

use crate::error::Error;
use crate::volume::VolumeState;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Where a stream is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Silence is on the wire while the ring fills to the start threshold (at
    /// stream start, after `flush()`, or after an underrun recovered).
    Buffering,
    /// Audio from the ring is on the wire.
    Playing,
    /// `pause()` is in effect: wire-correct silence, ring untouched.
    Paused,
    /// `drain()` was called and queued audio is still playing out.
    Draining,
    /// `drain()` was called and everything queued has been played.
    Drained,
    /// The stream was closed.
    Closed,
    /// The stream stopped on an error (an `Error` event preceded this).
    Failed,
}

impl StreamState {
    /// Numeric code used by the JNI layer.
    pub fn code(self) -> i32 {
        match self {
            Self::Buffering => 0,
            Self::Playing => 1,
            Self::Paused => 2,
            Self::Draining => 3,
            Self::Drained => 4,
            Self::Closed => 5,
            Self::Failed => 6,
        }
    }
}

/// Something that happened to the DAC or a stream.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum DacEvent {
    /// The stream's state changed.
    StateChanged(StreamState),
    /// The ring ran dry and the wire was padded. Sent once per run of
    /// underruns, not once per transfer.
    Underrun,
    /// The DAC did not take some queued audio (a timed-out or short transfer).
    /// It cannot be re-sent. Bit-perfect output ends at the first loss.
    LostFrames { frames: u64 },
    /// `is_bit_perfect()` changed.
    BitPerfectChanged(bool),
    /// The hardware volume or mute changed through this library.
    VolumeChanged(VolumeState),
    /// The DAC's feedback endpoint stopped producing valid reports; packet
    /// timing falls back to the last good value or the nominal rate.
    FeedbackStale,
    /// The stream (or a control request) failed.
    Error(Error),
    /// The device is gone.
    DeviceLost,
}

/// Receives [`DacEvent`]s. Must not block: it runs on the library's event
/// thread, and a blocked listener delays every later event.
pub trait DacListener: Send + Sync {
    fn on_event(&self, event: DacEvent);
}

impl<F> DacListener for F
where
    F: Fn(DacEvent) + Send + Sync,
{
    fn on_event(&self, event: DacEvent) {
        self(event)
    }
}

enum Envelope {
    Event(Arc<dyn DacListener>, DacEvent),
    Stop,
}

const QUEUE_CAPACITY: usize = 256;

/// Shared by the `Dac`, its streams and the output thread.
pub(crate) struct EventHub {
    tx: Sender<Envelope>,
    thread: Mutex<Option<JoinHandle<()>>>,
    dac_listener: Mutex<Option<Arc<dyn DacListener>>>,
    stream_listener: Mutex<Option<Arc<dyn DacListener>>>,
    /// Device events from before any listener existed (the start-up volume
    /// change happens inside `Dac::open`), delivered to the first DAC listener.
    pending_dac_events: Mutex<Vec<DacEvent>>,
    device_lost: AtomicBool,
}

pub(crate) type EventSink = Arc<EventHub>;

impl EventHub {
    pub(crate) fn start() -> Arc<Self> {
        let (tx, rx) = crossbeam_channel::bounded(QUEUE_CAPACITY);
        let thread = std::thread::Builder::new()
            .name("usbdac-events".to_string())
            .spawn(move || dispatch(rx))
            .map_err(|error| log::error!("could not start the event thread: {error}"))
            .ok();
        Arc::new(Self {
            tx,
            thread: Mutex::new(thread),
            dac_listener: Mutex::new(None),
            stream_listener: Mutex::new(None),
            pending_dac_events: Mutex::new(Vec::new()),
            device_lost: AtomicBool::new(false),
        })
    }

    pub(crate) fn set_dac_listener(&self, listener: Option<Arc<dyn DacListener>>) {
        *self.dac_listener.lock() = listener.clone();
        if let Some(listener) = listener {
            let pending = std::mem::take(&mut *self.pending_dac_events.lock());
            for event in pending {
                self.send(listener.clone(), event);
            }
        }
    }

    /// An event about the device that happened before a listener could exist:
    /// kept for the first DAC listener, or sent now if there is one.
    pub(crate) fn emit_dac_or_hold(&self, event: DacEvent) {
        if self.dac_listener.lock().is_some() {
            self.emit_dac(event);
        } else {
            self.pending_dac_events.lock().push(event);
        }
    }

    pub(crate) fn set_stream_listener(&self, listener: Option<Arc<dyn DacListener>>) {
        *self.stream_listener.lock() = listener;
    }

    /// An event of the active stream: only the stream's listener sees it.
    pub(crate) fn emit_stream(&self, event: DacEvent) {
        let listener = self.stream_listener.lock().clone();
        if let Some(listener) = listener {
            self.send(listener, event);
        }
    }

    /// An event about the device (volume, device loss): the DAC-level listener
    /// and, if different, the active stream's listener.
    pub(crate) fn emit_dac(&self, event: DacEvent) {
        if event == DacEvent::DeviceLost {
            self.device_lost.store(true, Ordering::Release);
        }
        let dac = self.dac_listener.lock().clone();
        let stream = self.stream_listener.lock().clone();
        match (dac, stream) {
            (Some(dac), Some(stream)) => {
                if Arc::ptr_eq(&dac, &stream) {
                    self.send(dac, event);
                } else {
                    self.send(dac, event.clone());
                    self.send(stream, event);
                }
            }
            (Some(listener), None) | (None, Some(listener)) => self.send(listener, event),
            (None, None) => {}
        }
    }

    fn send(&self, listener: Arc<dyn DacListener>, event: DacEvent) {
        match self.tx.try_send(Envelope::Event(listener, event)) {
            Ok(()) => {}
            Err(TrySendError::Full(Envelope::Event(_, dropped))) => {
                log::warn!("event queue full; dropping {dropped:?}");
            }
            Err(_) => {}
        }
    }

    /// Whether a `DeviceLost` has been reported.
    pub(crate) fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Acquire)
    }

    /// Stops the event thread after delivering what is queued. Idempotent.
    pub(crate) fn shutdown(&self) {
        let thread = self.thread.lock().take();
        if let Some(thread) = thread {
            let _ = self.tx.send(Envelope::Stop);
            // A listener that closes the Dac runs on the event thread itself,
            // which cannot wait for its own exit: let it finish on its own.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
        *self.dac_listener.lock() = None;
        *self.stream_listener.lock() = None;
    }
}

fn dispatch(rx: Receiver<Envelope>) {
    while let Ok(envelope) = rx.recv() {
        match envelope {
            Envelope::Event(listener, event) => {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    listener.on_event(event);
                }));
                if result.is_err() {
                    log::error!("a DacListener panicked; the event was dropped");
                }
            }
            Envelope::Stop => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PlMutex;
    use std::time::Duration;

    #[derive(Default)]
    struct Collector(PlMutex<Vec<(String, DacEvent)>>);

    impl DacListener for Collector {
        fn on_event(&self, event: DacEvent) {
            let thread = std::thread::current().name().unwrap_or("?").to_string();
            self.0.lock().push((thread, event));
        }
    }

    fn wait_for(collector: &Collector, count: usize) {
        for _ in 0..200 {
            if collector.0.lock().len() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {count} events");
    }

    #[test]
    fn events_arrive_in_order_on_the_library_thread() {
        let hub = EventHub::start();
        let listener = Arc::new(Collector::default());
        hub.set_stream_listener(Some(listener.clone()));
        hub.emit_stream(DacEvent::Underrun);
        hub.emit_stream(DacEvent::LostFrames { frames: 7 });
        hub.emit_stream(DacEvent::StateChanged(StreamState::Playing));
        wait_for(&listener, 3);
        hub.shutdown();
        let seen = listener.0.lock();
        assert_eq!(
            seen.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>(),
            vec![
                DacEvent::Underrun,
                DacEvent::LostFrames { frames: 7 },
                DacEvent::StateChanged(StreamState::Playing)
            ]
        );
        assert!(seen.iter().all(|(thread, _)| thread == "usbdac-events"));
    }

    #[test]
    fn device_events_reach_both_listeners_once_each() {
        let hub = EventHub::start();
        let dac = Arc::new(Collector::default());
        let stream = Arc::new(Collector::default());
        hub.set_dac_listener(Some(dac.clone()));
        hub.set_stream_listener(Some(stream.clone()));
        hub.emit_dac(DacEvent::DeviceLost);
        wait_for(&dac, 1);
        wait_for(&stream, 1);
        hub.emit_stream(DacEvent::Underrun);
        wait_for(&stream, 2);
        hub.shutdown();
        assert_eq!(dac.0.lock().len(), 1, "stream events stay with the stream");
        assert_eq!(stream.0.lock().len(), 2);
    }

    #[test]
    fn one_listener_in_both_roles_is_not_called_twice() {
        let hub = EventHub::start();
        let both = Arc::new(Collector::default());
        hub.set_dac_listener(Some(both.clone()));
        hub.set_stream_listener(Some(both.clone()));
        hub.emit_dac(DacEvent::DeviceLost);
        wait_for(&both, 1);
        std::thread::sleep(Duration::from_millis(30));
        hub.shutdown();
        assert_eq!(both.0.lock().len(), 1);
    }

    #[test]
    fn a_panicking_listener_does_not_kill_delivery() {
        struct Boom(PlMutex<u32>);
        impl DacListener for Boom {
            fn on_event(&self, event: DacEvent) {
                *self.0.lock() += 1;
                if event == DacEvent::Underrun {
                    panic!("listener bug");
                }
            }
        }
        let hub = EventHub::start();
        let boom = Arc::new(Boom(PlMutex::new(0)));
        hub.set_stream_listener(Some(boom.clone()));
        hub.emit_stream(DacEvent::Underrun);
        hub.emit_stream(DacEvent::DeviceLost);
        for _ in 0..200 {
            if *boom.0.lock() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        hub.shutdown();
        assert_eq!(*boom.0.lock(), 2);
    }

    #[test]
    fn shutting_down_from_inside_a_listener_does_not_deadlock() {
        struct Closer(PlMutex<Option<Arc<EventHub>>>);
        impl DacListener for Closer {
            fn on_event(&self, _event: DacEvent) {
                if let Some(hub) = self.0.lock().take() {
                    hub.shutdown();
                }
            }
        }
        let hub = EventHub::start();
        let closer = Arc::new(Closer(PlMutex::new(Some(hub.clone()))));
        hub.set_stream_listener(Some(closer.clone()));
        hub.emit_stream(DacEvent::Underrun);
        for _ in 0..200 {
            if closer.0.lock().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(closer.0.lock().is_none(), "the listener ran and shut the hub down");
        hub.shutdown();
    }

    #[test]
    fn an_event_from_before_any_listener_goes_to_the_first_dac_listener_once() {
        let hub = EventHub::start();
        let startup = DacEvent::VolumeChanged(VolumeState { db: -40.0, fraction: 0.5, muted: None });
        hub.emit_dac_or_hold(startup.clone());
        // Held, not lost, while nobody listens (a stream listener is not a DAC one).
        let stream = Arc::new(Collector::default());
        hub.set_stream_listener(Some(stream.clone()));
        std::thread::sleep(Duration::from_millis(20));
        assert!(stream.0.lock().is_empty());

        let first = Arc::new(Collector::default());
        hub.set_dac_listener(Some(first.clone()));
        wait_for(&first, 1);
        assert_eq!(first.0.lock()[0].1, startup);

        // Delivered once: a later listener does not see it again.
        let second = Arc::new(Collector::default());
        hub.set_dac_listener(Some(second.clone()));
        std::thread::sleep(Duration::from_millis(30));
        hub.shutdown();
        assert!(second.0.lock().is_empty());
        assert_eq!(first.0.lock().len(), 1);
    }

    #[test]
    fn an_event_with_a_listener_present_is_sent_at_once() {
        let hub = EventHub::start();
        let listener = Arc::new(Collector::default());
        hub.set_dac_listener(Some(listener.clone()));
        hub.emit_dac_or_hold(DacEvent::DeviceLost);
        wait_for(&listener, 1);
        hub.shutdown();
    }

    #[test]
    fn without_a_listener_events_are_discarded() {
        let hub = EventHub::start();
        hub.emit_stream(DacEvent::Underrun);
        hub.emit_dac(DacEvent::DeviceLost);
        hub.shutdown();
    }
}
