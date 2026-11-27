//! Ordered volume control without blocking a UI or an async executor.
use crate::{Error, Result, VolumeControl, VolumeState};
use crossbeam_channel::{bounded, Sender, TrySendError};
use parking_lot::{Condvar, Mutex};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

type Job = Box<dyn FnOnce(&VolumeControl) + Send>;

/// A bounded FIFO worker. Submission fails with [`Error::Busy`] when 32
/// requests are waiting. Accepted operations use the same verified hardware
/// writes, rollback and event delivery as [`VolumeControl`]. Close waits for
/// an executing control transfer; queued operations then fail as closed.
#[derive(Clone)]
pub struct AsyncVolumeControl {
    sender: Sender<Job>,
}

impl AsyncVolumeControl {
    pub(crate) fn new(control: VolumeControl) -> Result<Self> {
        let (sender, receiver) = bounded::<Job>(32);
        std::thread::Builder::new()
            .name("usbdac-volume".into())
            .spawn(move || {
                for job in receiver {
                    job(&control);
                }
            })
            .map_err(|e| Error::Control(format!("starting volume worker: {e}")))?;
        Ok(Self { sender })
    }

    fn submit<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&VolumeControl) -> Result<T> + Send + 'static,
    ) -> Result<VolumeOperation<T>> {
        let shared = Arc::new(Completion {
            state: Mutex::new((None, None, false)),
            ready: Condvar::new(),
        });
        let completion = Arc::clone(&shared);
        let job: Job = Box::new(move |control| {
            let result = operation(control);
            let waker = {
                let mut state = completion.state.lock();
                state.0 = Some(result);
                state.1.take()
            };
            completion.ready.notify_all();
            if let Some(waker) = waker {
                waker.wake();
            }
        });
        self.sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(_) => Error::Busy,
            TrySendError::Disconnected(_) => Error::InvalidState("volume worker stopped".into()),
        })?;
        Ok(VolumeOperation { shared })
    }

    pub fn get(&self) -> Result<VolumeOperation<VolumeState>> {
        self.submit(VolumeControl::get)
    }
    pub fn set_db(&self, db: f64) -> Result<VolumeOperation<VolumeState>> {
        self.submit(move |v| v.set_db(db))
    }
    pub fn set_fraction(&self, fraction: f64) -> Result<VolumeOperation<VolumeState>> {
        self.submit(move |v| v.set_fraction(fraction))
    }
    pub fn step(&self, direction: i8, steps: u8) -> Result<VolumeOperation<VolumeState>> {
        self.submit(move |v| v.step(direction, steps))
    }
    pub fn set_mute(&self, muted: bool) -> Result<VolumeOperation<VolumeState>> {
        self.submit(move |v| v.set_mute(muted))
    }
    pub fn hand_back(&self) -> Result<VolumeOperation<bool>> {
        self.submit(VolumeControl::hand_back)
    }
}

struct Completion<T> {
    state: Mutex<(Option<Result<T>>, Option<Waker>, bool)>,
    ready: Condvar,
}

/// An accepted volume request: `.await`, [`try_result`](Self::try_result) or
/// [`wait`](Self::wait) retrieves its result exactly once. Dropping this handle
/// leaves the accepted operation queued, preserving write ordering.
#[must_use]
pub struct VolumeOperation<T> {
    shared: Arc<Completion<T>>,
}

impl<T> VolumeOperation<T> {
    pub fn try_result(&mut self) -> Option<Result<T>> {
        let mut state = self.shared.state.lock();
        if state.2 {
            return Some(Err(consumed()));
        }
        let result = state.0.take();
        if result.is_some() {
            state.2 = true;
        }
        result
    }
    /// Block the calling thread. Do not call from an async executor or UI thread.
    pub fn wait(self) -> Result<T> {
        let mut state = self.shared.state.lock();
        loop {
            if state.2 {
                return Err(consumed());
            }
            if let Some(result) = state.0.take() {
                state.2 = true;
                return result;
            }
            self.shared.ready.wait(&mut state);
        }
    }
}

impl<T> Future for VolumeOperation<T> {
    type Output = Result<T>;
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.state.lock();
        if state.2 {
            return Poll::Ready(Err(consumed()));
        }
        if let Some(result) = state.0.take() {
            state.2 = true;
            Poll::Ready(result)
        } else {
            state.1 = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

fn consumed() -> Error {
    Error::InvalidState("volume result was already consumed".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn polling_then_waiting_returns_a_consumed_error() {
        let mut operation = VolumeOperation {
            shared: Arc::new(Completion {
                state: Mutex::new((Some(Ok(7u8)), None, false)),
                ready: Condvar::new(),
            }),
        };
        assert_eq!(operation.try_result(), Some(Ok(7)));
        assert!(matches!(operation.wait(), Err(Error::InvalidState(_))));
    }
}
