//! Handles: every `Long` the Java side holds is an id into these tables, never
//! a pointer. A stale or foreign id is an `invalid_argument` error, not a crash.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use usbdac_core::DacCaptureStream;
use usbdac_core::{Dac, DacStream, Error, StreamFormat};

/// One counter for every kind of handle, so a DAC id is never also a valid
/// stream id.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Negotiated formats kept per DAC, so `streamOpen` can take a `format_id`.
const FORMATS_KEPT: usize = 16;

/// Each DAC owns a distinct table; tokens share the global handle namespace.
#[derive(Default)]
struct Gen1Snapshots(Vec<(u64, usbdac_core::scarlett::Gen1Readback)>);
impl Gen1Snapshots {
    fn remember(&mut self, snapshot: usbdac_core::scarlett::Gen1Readback) -> u64 {
        let id=next_id();
        self.0.push((id,snapshot));
        if self.0.len()>8 { self.0.remove(0); }
        id
    }
    fn get(&self, id: i64) -> Result<usbdac_core::scarlett::Gen1Readback, Error> {
        let key=u64::try_from(id).map_err(|_|stale("first-generation snapshot",id))?;
        self.0.iter().find(|(candidate,_)|*candidate==key).map(|(_,v)|v.clone())
            .ok_or_else(||stale("first-generation snapshot",id))
    }
}

pub struct DacEntry {
    pub dac: Dac,
    formats: Mutex<Vec<(u64, StreamFormat)>>,
    streams: Mutex<Vec<u64>>,
    routing: Mutex<Vec<(u64, usbdac_core::routing::RoutingSnapshot)>>,
    scarlett_mux: Mutex<Vec<(u64, usbdac_core::scarlett::MuxSnapshot)>>,
    fcp: Mutex<Vec<(u64, usbdac_core::scarlett::FcpReadback)>>,
    gen1: Mutex<Gen1Snapshots>,
}

impl DacEntry {
    pub fn new(dac: Dac) -> Self {
        Self {
            dac,
            formats: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
            routing: Mutex::new(Vec::new()),
            scarlett_mux: Mutex::new(Vec::new()),
            fcp: Mutex::new(Vec::new()),
            gen1: Mutex::new(Gen1Snapshots::default()),
        }
    }

    /// Snapshots never cross DAC owners and disappear with this entry. Bound retained USB
    /// descriptor memory; callers rediscover when an old snapshot has been evicted.
    pub fn remember_routing(&self, snapshot: usbdac_core::routing::RoutingSnapshot) -> Result<u64, Error> {
        // Lock registration through insertion, so close either invalidates this result
        // or has already removed the owner and rejects insertion.
        let dacs = DACS.lock();
        if !dacs.values().any(|entry| std::ptr::eq(Arc::as_ptr(entry), self)) {
            return Err(Error::InvalidState("DAC closed during routing discovery".into()));
        }
        let id = next_id();
        let mut snapshots = self.routing.lock();
        snapshots.push((id, snapshot));
        if snapshots.len() > 8 { snapshots.remove(0); }
        Ok(id)
    }

    pub fn routing(&self, id: i64) -> Result<usbdac_core::routing::RoutingSnapshot, Error> {
        let id = u64::try_from(id).map_err(|_| stale("routing snapshot", id))?;
        self.routing.lock().iter().find(|(key, _)| *key == id)
            .map(|(_, snapshot)| snapshot.clone())
            .ok_or_else(|| stale("routing snapshot", id as i64))
    }

    /// Retain only native-observed raw banks, scoped to this open DAC owner.
    pub fn remember_scarlett_mux(&self, snapshot: usbdac_core::scarlett::MuxSnapshot) -> Result<u64, Error> {
        let dacs = DACS.lock();
        if !dacs.values().any(|entry| std::ptr::eq(Arc::as_ptr(entry), self)) {
            return Err(Error::InvalidState("DAC closed during Scarlett mux read".into()));
        }
        let id = next_id();
        let mut snapshots = self.scarlett_mux.lock();
        snapshots.push((id, snapshot));
        if snapshots.len() > 8 { snapshots.remove(0); }
        Ok(id)
    }

    pub fn scarlett_mux(&self, id: i64) -> Result<usbdac_core::scarlett::MuxSnapshot, Error> {
        let id = u64::try_from(id).map_err(|_| stale("Scarlett mux snapshot", id))?;
        self.scarlett_mux.lock().iter().find(|(key, _)| *key == id)
            .map(|(_, snapshot)| snapshot.clone())
            .ok_or_else(|| stale("Scarlett mux snapshot", id as i64))
    }

    /// Retain native-observed FCP state only, bounded to eight snapshots per owner.
    pub fn remember_fcp(&self, snapshot: usbdac_core::scarlett::FcpReadback) -> Result<u64, Error> {
        let dacs = DACS.lock();
        if !dacs.values().any(|entry| std::ptr::eq(Arc::as_ptr(entry), self)) {
            return Err(Error::InvalidState("DAC closed during FCP read".into()));
        }
        let id = next_id();
        let mut snapshots = self.fcp.lock();
        snapshots.push((id, snapshot));
        if snapshots.len() > 8 { snapshots.remove(0); }
        Ok(id)
    }

    pub fn fcp(&self, id: i64) -> Result<usbdac_core::scarlett::FcpReadback, Error> {
        let key = u64::try_from(id).map_err(|_| stale("FCP snapshot", id))?;
        self.fcp.lock().iter().find(|(candidate, _)| *candidate == key)
            .map(|(_, snapshot)| snapshot.clone()).ok_or_else(|| stale("FCP snapshot", id))
    }

    /// Only explicitly retained first-generation native readback grants an edit token.
    pub fn remember_gen1(&self, snapshot: usbdac_core::scarlett::Gen1Readback) -> Result<u64, Error> {
        let dacs = DACS.lock();
        if !dacs.values().any(|entry| std::ptr::eq(Arc::as_ptr(entry), self)) {
            return Err(Error::InvalidState("DAC closed during first-generation read".into()));
        }
        Ok(self.gen1.lock().remember(snapshot))
    }

    pub fn gen1(&self, id: i64) -> Result<usbdac_core::scarlett::Gen1Readback, Error> {
        self.gen1.lock().get(id)
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
type CaptureTable = HashMap<u64, (u64, Arc<DacCaptureStream>)>;
static CAPTURES: Lazy<Mutex<CaptureTable>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Publish both directions under the owner lock. DAC close either collects
/// the complete pair or makes this fail, dropping and retiring both workers.
pub fn insert_duplex(dac: &Arc<DacEntry>, playback: DacStream, capture: DacCaptureStream) -> Result<(u64, u64), Error> {
    let dacs = DACS.lock();
    let owner = dacs.iter().find(|(_, entry)| Arc::ptr_eq(entry, dac)).map(|(id, _)| *id)
        .ok_or_else(|| Error::InvalidState("DAC closed while duplex was opening".into()))?;
    let output = next_id();
    let input = next_id();
    STREAMS.lock().insert(output, (owner, Arc::new(playback)));
    CAPTURES.lock().insert(input, (owner, Arc::new(capture)));
    dac.add_stream(output);
    Ok((output, input))
}

pub fn insert_capture(dac: &Arc<DacEntry>, stream: DacCaptureStream) -> Result<u64, Error> {
    // Keep DAC removal serialized through insertion; a close racing open must
    // either collect this capture or make insertion fail and drop the stream.
    let dacs = DACS.lock();
    let owner = dacs
        .iter()
        .find(|(_, entry)| Arc::ptr_eq(entry, dac))
        .map(|(id, _)| *id)
        .ok_or_else(|| Error::InvalidState("DAC closed while capture was opening".into()))?;
    let id = next_id();
    CAPTURES.lock().insert(id, (owner, Arc::new(stream)));
    Ok(id)
}

pub fn capture(id: i64) -> Result<Arc<DacCaptureStream>, Error> {
    u64::try_from(id)
        .ok()
        .and_then(|id| CAPTURES.lock().get(&id).map(|(_, stream)| stream.clone()))
        .ok_or_else(|| stale("capture", id))
}

pub fn remove_capture(id: i64) -> Result<Arc<DacCaptureStream>, Error> {
    u64::try_from(id)
        .ok()
        .and_then(|id| CAPTURES.lock().remove(&id).map(|(_, stream)| stream))
        .ok_or_else(|| stale("capture", id))
}

/// Call after DAC removal, which prevents further capture handle insertion.
pub fn remove_dac_captures(dac_id: i64) -> Vec<Arc<DacCaptureStream>> {
    let mut table = CAPTURES.lock();
    let ids: Vec<_> = table
        .iter()
        .filter(|(_, (owner, _))| *owner == dac_id as u64)
        .map(|(id, _)| *id)
        .collect();
    ids.into_iter()
        .filter_map(|id| table.remove(&id).map(|(_, stream)| stream))
        .collect()
}

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

/// Resolve only a stream registered to this exact DAC owner.
pub fn stream_for_dac(dac: &Arc<DacEntry>, id: i64) -> Result<Arc<DacStream>, Error> {
    let owners = DACS.lock();
    let owner = owners.iter().find(|(_, entry)| Arc::ptr_eq(entry, dac)).map(|(id, _)| *id)
        .ok_or_else(|| Error::InvalidState("DAC closed during reconfiguration".into()))?;
    let key = u64::try_from(id).map_err(|_| stale("stream", id))?;
    STREAMS.lock().get(&key).filter(|(registered, _)| *registered == owner)
        .map(|(_, stream)| stream.clone()).ok_or_else(|| Error::InvalidArgument("stream is stale or belongs to another DAC".into()))
}

/// Commit replacement only while both the DAC and old registration still exist.
/// On failure the unregistered replacement drops/closes and cannot become an orphan.
pub fn replace_stream(dac: &Arc<DacEntry>, previous: i64, id: u64, stream: DacStream) -> Result<(), Error> {
    let owners = DACS.lock();
    let owner = owners.iter().find(|(_, entry)| Arc::ptr_eq(entry, dac)).map(|(id, _)| *id)
        .ok_or_else(|| Error::InvalidState("DAC closed before replacement registration".into()))?;
    let previous = u64::try_from(previous).map_err(|_| stale("stream", previous))?;
    let mut streams = STREAMS.lock();
    if !streams.get(&previous).is_some_and(|(registered, _)| *registered == owner) {
        return Err(Error::InvalidState("old stream closed during reconfiguration".into()));
    }
    streams.remove(&previous);
    streams.insert(id, (owner, Arc::new(stream)));
    dac.forget_stream(previous); dac.add_stream(id);
    Ok(())
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
    fn gen1_tokens_are_owner_scoped_bounded_and_reject_invalid_handles() {
        use usbdac_core::scarlett::{Gen1Model,Gen1Readback};
        let snapshot=Gen1Readback {model:Gen1Model::Forte,controls:vec![],
            source_offsets:[0,4,6,6,6],matrix_sources:7,output_sources:0};
        let mut owner=Gen1Snapshots::default();
        let foreign=Gen1Snapshots::default();
        let token=owner.remember(snapshot.clone());
        assert_eq!(owner.get(token as i64).unwrap(),snapshot);
        assert!(foreign.get(token as i64).is_err());
        assert!(owner.get(-1).is_err());
        assert!(owner.get(0).is_err());
        for _ in 0..8 { owner.remember(snapshot.clone()); }
        assert!(owner.get(token as i64).is_err());
        assert_eq!(owner.0.len(),8);
        let fresh=owner.remember(snapshot.clone());
        assert_eq!(owner.get(fresh as i64).unwrap(),snapshot);
    }

    #[test]
    fn ids_are_unique_across_kinds_and_stale_ids_are_errors() {
        let a = next_id();
        let b = next_id();
        assert_ne!(a, b);
        assert!(matches!(dac(a as i64), Err(Error::InvalidArgument(_))));
        assert!(matches!(stream(b as i64), Err(Error::InvalidArgument(_))));
        assert!(matches!(dac(-5), Err(Error::InvalidArgument(_))));
        assert!(matches!(remove_stream(0), Err(Error::InvalidArgument(_))));
        assert!(matches!(capture(a as i64), Err(Error::InvalidArgument(_))));
        assert!(matches!(remove_capture(-1), Err(Error::InvalidArgument(_))));
        assert!(remove_dac_captures(0).is_empty());
        assert!(remove_dac(0).is_err());
    }
}
