//! Handles: every `Long` the Java side holds is an id into these tables, never
//! a pointer. A stale or foreign id is an `invalid_argument` error, not a crash.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use usbdac_core::{Dac, DacStream, Error, StreamFormat};

/// One counter for every kind of handle, so a DAC id is never also a valid
/// stream id.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Negotiated formats kept per DAC, so `streamOpen` can take a `format_id`.
const FORMATS_KEPT: usize = 16;

pub struct DacEntry {
    pub dac: Dac,
    formats: Mutex<Vec<(u64, StreamFormat)>>,
    streams: Mutex<Vec<u64>>,
}

impl DacEntry {
    pub fn new(dac: Dac) -> Self {
        Self {
            dac,
            formats: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
        }
    }

    pub fn remember_format(&self, format: StreamFormat) -> u64 {
        let id = next_id();
        let mut formats = self.formats.lock();
        formats.push((id, format));
        if formats.len() > FORMATS_KEPT {
            formats.remove(0);
        }
        id
    }

    pub fn format(&self, id: u64) -> Option<StreamFormat> {
        self.formats
            .lock()
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map(|(_, format)| format.clone())
    }

    pub fn add_stream(&self, id: u64) {
        self.streams.lock().push(id);
    }

    pub fn forget_stream(&self, id: u64) {
        self.streams.lock().retain(|candidate| *candidate != id);
    }

    pub fn take_streams(&self) -> Vec<u64> {
        std::mem::take(&mut *self.streams.lock())
    }
}

static DACS: Lazy<Mutex<HashMap<u64, Arc<DacEntry>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
/// Stream id to (owning DAC id, stream).
type StreamTable = HashMap<u64, (u64, Arc<DacStream>)>;

static STREAMS: Lazy<Mutex<StreamTable>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn stale(kind: &str, id: i64) -> Error {
    Error::InvalidArgument(format!("{kind} handle {id} is not open"))
}

pub fn insert_dac(entry: DacEntry) -> u64 {
    let id = next_id();
    DACS.lock().insert(id, Arc::new(entry));
    id
}

pub fn dac(id: i64) -> Result<Arc<DacEntry>, Error> {
    u64::try_from(id)
        .ok()
        .and_then(|id| DACS.lock().get(&id).cloned())
        .ok_or_else(|| stale("DAC", id))
}

/// Removes a DAC and the streams opened from it; returns the entry (so the
/// caller can close it) and the streams (so they can be closed first).
pub fn remove_dac(id: i64) -> Result<(Arc<DacEntry>, Vec<Arc<DacStream>>), Error> {
    let entry = u64::try_from(id)
        .ok()
        .and_then(|id| DACS.lock().remove(&id))
        .ok_or_else(|| stale("DAC", id))?;
    let mut streams = Vec::new();
    let mut table = STREAMS.lock();
    for stream_id in entry.take_streams() {
        if let Some((_, stream)) = table.remove(&stream_id) {
            streams.push(stream);
        }
    }
    Ok((entry, streams))
}

pub fn insert_stream(dac: &Arc<DacEntry>, stream: DacStream) -> u64 {
    let id = next_id();
    let owner = DACS
        .lock()
        .iter()
        .find(|(_, entry)| Arc::ptr_eq(entry, dac))
        .map(|(id, _)| *id)
        .unwrap_or(0);
    STREAMS.lock().insert(id, (owner, Arc::new(stream)));
    dac.add_stream(id);
    id
}

pub fn stream(id: i64) -> Result<Arc<DacStream>, Error> {
    u64::try_from(id)
        .ok()
        .and_then(|id| STREAMS.lock().get(&id).map(|(_, stream)| stream.clone()))
        .ok_or_else(|| stale("stream", id))
}

pub fn remove_stream(id: i64) -> Result<Arc<DacStream>, Error> {
    let (owner, stream) = u64::try_from(id)
        .ok()
        .and_then(|id| STREAMS.lock().remove(&id))
        .ok_or_else(|| stale("stream", id))?;
    if let Some(entry) = DACS.lock().get(&owner) {
        entry.forget_stream(id as u64);
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_across_kinds_and_stale_ids_are_errors() {
        let a = next_id();
        let b = next_id();
        assert_ne!(a, b);
        assert!(matches!(dac(a as i64), Err(Error::InvalidArgument(_))));
        assert!(matches!(stream(b as i64), Err(Error::InvalidArgument(_))));
        assert!(matches!(dac(-5), Err(Error::InvalidArgument(_))));
        assert!(matches!(remove_stream(0), Err(Error::InvalidArgument(_))));
        assert!(remove_dac(0).is_err());
    }
}
