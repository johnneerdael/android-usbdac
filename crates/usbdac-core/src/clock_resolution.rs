//! Which Clock Source feeds a streaming terminal (UAC 2.0, section 4.7.2).
//!
//! A terminal's `bCSourceID` names the clock entity that drives it. That entity
//! is a Clock Source (0x0A) on simple DACs, but it can also be a Clock Selector
//! (0x0B) that chooses between several clocks, or a Clock Multiplier (0x0C) fed
//! by another clock entity. Setting and verifying the sample rate has to happen
//! on the Clock Source at the end of that chain, not on whichever source the
//! descriptor happens to list first.
//!
//! This module is plain data and callbacks so every case can be unit-tested
//! without libusb. The caller supplies the AudioControl class-specific
//! descriptor bytes and a callback that asks the device which pin a Clock
//! Selector currently has selected.

use crate::descriptors::AudioControlParser;
use std::collections::{HashMap, HashSet};
use std::fmt;

const USB_DT_INTERFACE: u8 = 0x04;
const USB_DT_CS_INTERFACE: u8 = 0x24;
const USB_CLASS_AUDIO: u8 = 0x01;
const USB_SUBCLASS_AUDIOCONTROL: u8 = 0x01;
const UAC2_INPUT_TERMINAL: u8 = 0x02;
const UAC2_OUTPUT_TERMINAL: u8 = 0x03;
const UAC2_CLOCK_SOURCE: u8 = 0x0a;
const UAC2_CLOCK_SELECTOR: u8 = 0x0b;
const UAC2_CLOCK_MULTIPLIER: u8 = 0x0c;

/// Upper bound on hops. Real topologies are two or three entities deep; this
/// only stops a pathological descriptor from walking for long.
const MAX_CLOCK_PATH: usize = 16;

/// How the clock the library programs the sample rate on was reached, for
/// diagnosing a DAC with more than one clock. Read-only: nothing here changes
/// what the library does.
///
/// UAC 2.0 routes a streaming terminal to its clock through Clock Selectors
/// (several inputs, one active) and Clock Multipliers, ending at a Clock Source
/// (section 4.7.2). The sampling frequency is set and read back on that source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockChoice {
    /// Interface number of the AudioControl interface the clock requests go to.
    pub control_interface: u8,
    /// Entity ID of the Clock Source the sample rate is set and verified on.
    pub source_id: u8,
    /// The entities followed, in order: the streaming terminal, then each clock
    /// entity down to the Clock Source (the last hop, whose `id` is `source_id`).
    ///
    /// For [`ClockResolution::FallbackFirstSource`] nothing was followed, so the
    /// path is the Clock Source alone.
    pub path: Vec<ClockHop>,
    pub resolution: ClockResolution,
}

/// How [`ClockChoice::source_id`] was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockResolution {
    /// The stream's terminal was traced to the Clock Source through the
    /// descriptors (and, at a Clock Selector with several inputs, the pin the
    /// device reports as selected).
    Resolved,
    /// The topology could not be traced (no terminal link, a malformed or
    /// looping entity, a selector that would not say which pin is active), so
    /// the first Clock Source in descriptor order was used. On a DAC with more
    /// than one clock the rate may then be set on the wrong one.
    FallbackFirstSource,
}

impl ClockResolution {
    /// Stable identifier: `resolved`, `fallback_first_source`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::FallbackFirstSource => "fallback_first_source",
        }
    }
}

/// One entity on the path from a streaming terminal to its Clock Source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockHop {
    /// The entity ID (the terminal ID for [`ClockHopKind::Terminal`]).
    pub id: u8,
    pub kind: ClockHopKind,
}

/// What a [`ClockHop`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClockHopKind {
    /// The streaming terminal the path starts at.
    Terminal,
    /// A Clock Selector. Pins are 1-based. A selector with one input has
    /// nothing to choose and is reported as `selected_pin: 1, pin_count: 1`
    /// without being asked; with several, the pin is what the device reported.
    Selector { selected_pin: u8, pin_count: u8 },
    /// A Clock Multiplier (one clock input).
    Multiplier,
    /// A Clock Source: the end of the path.
    Source,
}

impl ClockHopKind {
    /// Stable identifier: `terminal`, `selector`, `multiplier`, `source`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Selector { .. } => "selector",
            Self::Multiplier => "multiplier",
            Self::Source => "source",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClockEntity {
    Source,
    /// Input pins in descriptor order. Pin numbers on the wire are 1-based.
    Selector(Vec<u8>),
    Multiplier(u8),
    /// A selector or multiplier descriptor that is too short to trust.
    Malformed,
}

/// The clock part of one AudioControl interface's class-specific descriptors.
#[derive(Debug, Default)]
pub(crate) struct ClockGraph {
    /// Terminal ID to the clock entity ID named by its `bCSourceID`.
    terminal_clock: HashMap<u8, u8>,
    entities: HashMap<u8, ClockEntity>,
    first_source: Option<u8>,
    // Strict selectable topology is separate from the legacy sparse fallback.
    exact_sources: HashSet<u8>,
    selector_controls: HashMap<u8, u8>,
    duplicate_ids: HashSet<u8>,
}

impl ClockGraph {
    /// Scan class-specific AudioControl descriptors (the interface's `extra`
    /// bytes). Unknown or truncated descriptors are skipped, never fatal.
    pub(crate) fn parse(descriptors: &[u8]) -> Self {
        let mut graph = Self::default();
        let mut index = 0usize;
        let mut seen_ids = HashSet::new();
        while index + 2 < descriptors.len() {
            let length = descriptors[index] as usize;
            if length < 3 || index + length > descriptors.len() {
                break;
            }
            let descriptor = &descriptors[index..index + length];
            index += length;
            if descriptor[1] != USB_DT_CS_INTERFACE {
                continue;
            }
            if length >= 4
                && matches!(
                    descriptor[2],
                    UAC2_INPUT_TERMINAL
                        | UAC2_OUTPUT_TERMINAL
                        | UAC2_CLOCK_SOURCE
                        | UAC2_CLOCK_SELECTOR
                        | UAC2_CLOCK_MULTIPLIER
                )
                && !seen_ids.insert(descriptor[3])
            {
                graph.duplicate_ids.insert(descriptor[3]);
            }
            match descriptor[2] {
                // bTerminalID at 3; bCSourceID at 7 (Input Terminal, 4.7.2.4).
                UAC2_INPUT_TERMINAL if length >= 8 => {
                    graph.terminal_clock.insert(descriptor[3], descriptor[7]);
                }
                // bTerminalID at 3; bSourceID at 7 and bCSourceID at 8 (Output
                // Terminal, 4.7.2.5).
                UAC2_OUTPUT_TERMINAL if length >= 9 => {
                    graph.terminal_clock.insert(descriptor[3], descriptor[8]);
                }
                UAC2_CLOCK_SOURCE if length >= 4 => {
                    graph.entities.insert(descriptor[3], ClockEntity::Source);
                    graph.first_source.get_or_insert(descriptor[3]);
                    if length >= 8 && descriptor[3] != 0 {
                        graph.exact_sources.insert(descriptor[3]);
                    }
                }
                UAC2_CLOCK_SELECTOR if length >= 4 => {
                    if length >= 5 {
                        let pins = usize::from(descriptor[4]);
                        if length >= 7 + pins {
                            graph
                                .selector_controls
                                .insert(descriptor[3], descriptor[5 + pins]);
                        }
                    }
                    let entity = AudioControlParser
                        .parse_clock_selector(descriptor)
                        .map(|selector| ClockEntity::Selector(selector.ba_c_source_id))
                        .unwrap_or(ClockEntity::Malformed);
                    graph.entities.insert(descriptor[3], entity);
                }
                UAC2_CLOCK_MULTIPLIER if length >= 4 => {
                    let entity = AudioControlParser
                        .parse_clock_multiplier(descriptor)
                        .map(|multiplier| ClockEntity::Multiplier(multiplier.b_c_source_id))
                        .unwrap_or(ClockEntity::Malformed);
                    graph.entities.insert(descriptor[3], entity);
                }
                _ => {}
            }
        }
        graph
    }

    /// Enumerate exact selectable UAC2 paths, current pin first at every branch.
    /// Reads are cached per selector; a failed/illegal read cannot be safely
    /// restored and therefore excludes that selector entirely. This never uses
    /// the legacy first-source or sparse-descriptor fallback. An unknown
    /// multiplier ratio is deliberately unsupported for selectable routes.
    pub(crate) fn enumerate_plans(
        &self,
        terminal_id: u8,
        mut read_pin: impl FnMut(u8) -> Result<u8, String>,
    ) -> Result<Vec<ClockPlan>, String> {
        if self.duplicate_ids.contains(&terminal_id) {
            return Err(format!("terminal {terminal_id} has a duplicate entity ID"));
        }
        let first = *self
            .terminal_clock
            .get(&terminal_id)
            .ok_or_else(|| format!("terminal {terminal_id} has no clock link"))?;
        let mut search = ClockSearch {
            graph: self,
            pins: HashMap::new(),
            read_pin: &mut read_pin,
            remaining: 4096,
            plans: Vec::new(),
        };
        search.walk(first, &mut Vec::new(), &mut Vec::new())?;
        if search.plans.is_empty() {
            return Err(format!(
                "terminal {terminal_id} has no exact selectable clock path"
            ));
        }
        Ok(search.plans)
    }

    /// True if any clock entity was described. Without one (UAC 1.0 has none)
    /// there is no clock topology to resolve.
    pub(crate) fn has_clock_entities(&self) -> bool {
        !self.entities.is_empty()
    }

    /// The first Clock Source in descriptor order: the last-resort answer when
    /// the topology cannot be resolved.
    pub(crate) fn first_clock_source(&self) -> Option<u8> {
        self.first_source
    }

    /// Walk from `terminal_id` to the Clock Source currently feeding it.
    ///
    /// `selected_pin` is asked for the 1-based current input pin of a Clock
    /// Selector, and only for a selector with more than one input. A selector
    /// with one input has nothing to choose, and a multiplier has a single
    /// clock input, so neither needs a request.
    ///
    /// An ID with no clock descriptor of its own is taken to be the source:
    /// sparse descriptors do exist, and the old behaviour of treating it as
    /// unusable sent the sample rate to the wrong clock.
    pub(crate) fn resolve_terminal_clock(
        &self,
        terminal_id: u8,
        mut selected_pin: impl FnMut(u8) -> Result<u8, String>,
    ) -> Result<ResolvedClock, ClockResolveError> {
        let first = *self
            .terminal_clock
            .get(&terminal_id)
            .ok_or(ClockResolveError::TerminalNotFound(terminal_id))?;
        if first == 0 {
            return Err(ClockResolveError::TerminalHasNoClock(terminal_id));
        }

        let mut path = vec![first];
        let mut hops = vec![ClockHop {
            id: terminal_id,
            kind: ClockHopKind::Terminal,
        }];
        loop {
            let current = *path.last().expect("path starts non-empty");
            match self.entities.get(&current) {
                None | Some(ClockEntity::Source) => {
                    let assumed_source = !self.entities.contains_key(&current);
                    hops.push(ClockHop {
                        id: current,
                        kind: ClockHopKind::Source,
                    });
                    return Ok(ResolvedClock {
                        clock_id: current,
                        path,
                        hops,
                        assumed_source,
                    });
                }
                Some(ClockEntity::Malformed) => {
                    return Err(ClockResolveError::MalformedEntity(current));
                }
                Some(ClockEntity::Multiplier(input)) => {
                    if *input == 0 {
                        return Err(ClockResolveError::MalformedEntity(current));
                    }
                    hops.push(ClockHop {
                        id: current,
                        kind: ClockHopKind::Multiplier,
                    });
                    path.push(*input);
                }
                Some(ClockEntity::Selector(inputs)) => {
                    let pin_count = u8::try_from(inputs.len()).unwrap_or(u8::MAX);
                    let (next, pin) = match inputs.as_slice() {
                        [] => return Err(ClockResolveError::SelectorWithoutInputs(current)),
                        [only] => (*only, 1),
                        many => {
                            let pin = selected_pin(current).map_err(|reason| {
                                ClockResolveError::PinUnavailable {
                                    selector: current,
                                    reason,
                                }
                            })?;
                            if pin == 0 || usize::from(pin) > many.len() {
                                return Err(ClockResolveError::PinOutOfRange {
                                    selector: current,
                                    pin,
                                    inputs: many.len(),
                                });
                            }
                            (many[usize::from(pin) - 1], pin)
                        }
                    };
                    if next == 0 {
                        return Err(ClockResolveError::MalformedEntity(current));
                    }
                    hops.push(ClockHop {
                        id: current,
                        kind: ClockHopKind::Selector {
                            selected_pin: pin,
                            pin_count,
                        },
                    });
                    path.push(next);
                }
            }
            // An entity may appear once; reaching one again is a loop.
            let last = *path.last().expect("path is non-empty");
            if path[..path.len() - 1].contains(&last) {
                return Err(ClockResolveError::Loop(last));
            }
            if path.len() > MAX_CLOCK_PATH {
                return Err(ClockResolveError::TooDeep);
            }
        }
    }

    /// The clock the sample rate is programmed on for the stream whose
    /// AudioStreaming interface links `terminal_link`, on the AudioControl
    /// interface `control_interface`.
    ///
    /// The terminal is traced to its Clock Source with
    /// [`resolve_terminal_clock`](Self::resolve_terminal_clock). When that
    /// fails (or the stream names no terminal) the first Clock Source in
    /// descriptor order is used instead, and the result says so. `Err` carries
    /// the reason when there is no clock to choose: the interface describes no
    /// clock entities, or none is a Clock Source to fall back on.
    pub(crate) fn select_clock(
        &self,
        control_interface: u8,
        terminal_link: Option<u8>,
        selected_pin: impl FnMut(u8) -> Result<u8, String>,
    ) -> Result<ClockSelection, String> {
        // UAC 1.0 describes no clock entities, and its terminal descriptors must
        // not be read as if they named one.
        if !self.has_clock_entities() {
            return Err("the AudioControl interface describes no clock entities".to_string());
        }
        let unresolved = match terminal_link {
            None => "the stream has no bTerminalLink to trace its clock from".to_string(),
            Some(terminal) => match self.resolve_terminal_clock(terminal, selected_pin) {
                Ok(resolved) => {
                    return Ok(ClockSelection {
                        choice: ClockChoice {
                            control_interface,
                            source_id: resolved.clock_id,
                            path: resolved.hops,
                            resolution: ClockResolution::Resolved,
                        },
                        assumed_source: resolved.assumed_source,
                        unresolved: None,
                    });
                }
                Err(error) => error.to_string(),
            },
        };
        match self.first_clock_source() {
            Some(source_id) => Ok(ClockSelection {
                choice: ClockChoice {
                    control_interface,
                    source_id,
                    path: vec![ClockHop {
                        id: source_id,
                        kind: ClockHopKind::Source,
                    }],
                    resolution: ClockResolution::FallbackFirstSource,
                },
                assumed_source: false,
                unresolved: Some(unresolved),
            }),
            None => Err(unresolved),
        }
    }
}

/// One selector's snapshot and requested 1-based pin. Kept even for unchanged
/// selectors so opening a negotiated plan can reject intervening route changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClockPinPlan {
    pub selector: u8,
    pub original: u8,
    pub target: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClockPlan {
    pub source_id: u8,
    /// Root-to-source entity IDs, excluding the streaming terminal.
    pub path: Vec<u8>,
    /// Root-to-leaf selector order, including unchanged pins.
    pub selectors: Vec<ClockPinPlan>,
}

/// Transport boundary shared by plan application and hand-back. Implementations
/// must perform exact-length control transfers, including GET after each SET.
pub(crate) trait ClockPlanIo {
    fn read_pin(&mut self, selector: u8) -> Result<u8, String>;
    fn write_pin(&mut self, selector: u8, pin: u8) -> Result<(), String>;
    /// Check validity and requested sampling cadence on this exact source.
    /// Called after the whole path is selected, even when no selector changes.
    fn validate_source(&mut self, source: u8) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClockPlanError {
    pub reason: String,
    /// A non-empty list means the hardware route could not be restored.
    pub rollback_errors: Vec<String>,
}

impl fmt::Display for ClockPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)?;
        for error in &self.rollback_errors {
            write!(f, "; rollback: {error}")?;
        }
        Ok(())
    }
}

impl ClockPlan {
    /// Reject stale negotiation before any write. Apply downstream selectors
    /// first, verify each write, then validate the exact source. A failed SET
    /// may have reached hardware, so include it in the rollback attempt.
    pub(crate) fn apply(&self, io: &mut impl ClockPlanIo) -> Result<(), ClockPlanError> {
        let mut attempted = Vec::new();
        let result = (|| {
            for pin in &self.selectors {
                verify_pin(io, pin.selector, pin.original)?;
            }
            for pin in self
                .selectors
                .iter()
                .rev()
                .filter(|pin| pin.original != pin.target)
            {
                attempted.push(pin);
                io.write_pin(pin.selector, pin.target)?;
                verify_pin(io, pin.selector, pin.target)?;
            }
            // Re-read unchanged selectors too: a route change during apply
            // must not turn an unrelated source's validity into acceptance.
            for pin in &self.selectors {
                verify_pin(io, pin.selector, pin.target)?;
            }
            io.validate_source(self.source_id)
        })();
        match result {
            Ok(()) => Ok(()),
            Err(reason) => Err(ClockPlanError {
                reason,
                rollback_errors: restore_pins(io, attempted.into_iter().rev()),
            }),
        }
    }

    /// Explicit hand-back after a successful apply. Check the whole selected
    /// route before restoring anything, to avoid overwriting an external change.
    pub(crate) fn restore(&self, io: &mut impl ClockPlanIo) -> Result<(), ClockPlanError> {
        for pin in &self.selectors {
            verify_pin(io, pin.selector, pin.target).map_err(|reason| ClockPlanError {
                reason,
                rollback_errors: Vec::new(),
            })?;
        }
        let errors = restore_pins(io, self.selectors.iter().filter(|p| p.original != p.target));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ClockPlanError {
                reason: "clock hand-back failed".into(),
                rollback_errors: errors,
            })
        }
    }
}

fn verify_pin(io: &mut impl ClockPlanIo, selector: u8, expected: u8) -> Result<(), String> {
    let actual = io.read_pin(selector)?;
    if actual != expected {
        return Err(format!(
            "clock selector {selector} expected pin {expected}, read {actual}"
        ));
    }
    Ok(())
}

fn restore_pins<'a>(
    io: &mut impl ClockPlanIo,
    pins: impl Iterator<Item = &'a ClockPinPlan>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for pin in pins {
        if let Err(error) = io
            .write_pin(pin.selector, pin.original)
            .and_then(|()| verify_pin(io, pin.selector, pin.original))
        {
            errors.push(format!("selector {}: {error}", pin.selector));
        }
    }
    errors
}

struct ClockSearch<'a, F> {
    graph: &'a ClockGraph,
    pins: HashMap<u8, Option<u8>>,
    read_pin: &'a mut F,
    remaining: usize,
    plans: Vec<ClockPlan>,
}

impl<F: FnMut(u8) -> Result<u8, String>> ClockSearch<'_, F> {
    fn walk(
        &mut self,
        id: u8,
        path: &mut Vec<u8>,
        selectors: &mut Vec<ClockPinPlan>,
    ) -> Result<(), String> {
        // Bound work as well as path depth: shared DAG branches can otherwise
        // produce exponentially many paths despite per-path cycle detection.
        if self.remaining == 0 || self.plans.len() >= 256 {
            return Err("clock graph enumeration exceeds its bounded search budget".into());
        }
        self.remaining -= 1;
        if id == 0
            || self.graph.duplicate_ids.contains(&id)
            || path.len() >= MAX_CLOCK_PATH
            || path.contains(&id)
        {
            return Ok(());
        }
        path.push(id);
        match self.graph.entities.get(&id) {
            Some(ClockEntity::Source) if self.graph.exact_sources.contains(&id) => {
                self.plans.push(ClockPlan {
                    source_id: id,
                    path: path.clone(),
                    selectors: selectors.clone(),
                });
            }
            Some(ClockEntity::Selector(inputs))
                if !inputs.is_empty() && self.graph.selector_controls.contains_key(&id) =>
            {
                let controls = self.graph.selector_controls.get(&id).copied().unwrap_or(0) & 3;
                // UAC2: 00 absent, 01 read-only, 10 reserved, 11 read/write.
                if inputs.len() == 1 {
                    self.walk(inputs[0], path, selectors)?;
                } else if controls == 1 || controls == 3 {
                    let current = *self.pins.entry(id).or_insert_with(|| {
                        (self.read_pin)(id)
                            .ok()
                            .filter(|pin| *pin > 0 && usize::from(*pin) <= inputs.len())
                    });
                    if let Some(current) = current {
                        let order = std::iter::once(current).chain(
                            (1..=inputs.len() as u8).filter(|pin| controls == 3 && *pin != current),
                        );
                        for target in order {
                            selectors.push(ClockPinPlan {
                                selector: id,
                                original: current,
                                target,
                            });
                            self.walk(inputs[usize::from(target) - 1], path, selectors)?;
                            selectors.pop();
                        }
                    }
                }
            }
            // Missing/malformed entities and unresolved multiplier ratios do
            // not prove a programmable source and never produce a fallback.
            _ => {}
        }
        path.pop();
        Ok(())
    }
}

/// What [`ClockGraph::select_clock`] chose, with what the transport logs about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClockSelection {
    pub choice: ClockChoice,
    /// The source has no descriptor of its own and was taken to be one.
    pub assumed_source: bool,
    /// Why the topology could not be traced. Set exactly when the choice is
    /// [`ClockResolution::FallbackFirstSource`].
    pub unresolved: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedClock {
    pub clock_id: u8,
    /// Entity IDs from the terminal's `bCSourceID` to `clock_id`, inclusive.
    pub path: Vec<u8>,
    /// The same walk with what each entity is, starting at the terminal itself.
    pub hops: Vec<ClockHop>,
    /// `clock_id` has no descriptor of its own and was taken to be the source.
    pub assumed_source: bool,
}

impl ResolvedClock {
    /// Shared source IDs prove equal sample cadence only without an unknown
    /// multiplier ratio, and only when the source is actually described.
    pub(crate) fn proves_identity_cadence(&self) -> bool {
        !self.assumed_source
            && !self
                .hops
                .iter()
                .any(|hop| matches!(hop.kind, ClockHopKind::Multiplier))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClockResolveError {
    TerminalNotFound(u8),
    TerminalHasNoClock(u8),
    MalformedEntity(u8),
    SelectorWithoutInputs(u8),
    PinUnavailable {
        selector: u8,
        reason: String,
    },
    PinOutOfRange {
        selector: u8,
        pin: u8,
        inputs: usize,
    },
    Loop(u8),
    TooDeep,
}

impl fmt::Display for ClockResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TerminalNotFound(id) => {
                write!(f, "terminal {id} is not in the AudioControl descriptors")
            }
            Self::TerminalHasNoClock(id) => {
                write!(f, "terminal {id} names no clock (bCSourceID 0)")
            }
            Self::MalformedEntity(id) => write!(f, "clock entity {id} has a malformed descriptor"),
            Self::SelectorWithoutInputs(id) => write!(f, "clock selector {id} has no input pins"),
            Self::PinUnavailable { selector, reason } => {
                write!(
                    f,
                    "current pin of clock selector {selector} is unavailable: {reason}"
                )
            }
            Self::PinOutOfRange {
                selector,
                pin,
                inputs,
            } => write!(
                f,
                "clock selector {selector} reports pin {pin}, outside its {inputs} inputs"
            ),
            Self::Loop(id) => write!(f, "clock entity {id} feeds itself"),
            Self::TooDeep => write!(f, "clock topology is deeper than {MAX_CLOCK_PATH} entities"),
        }
    }
}

/// Concatenate the class-specific descriptors of one AudioControl interface
/// from a raw configuration descriptor. Used when the platform hands back no
/// `extra` bytes for the interface.
pub(crate) fn audio_control_descriptors_from_config(
    raw: &[u8],
    ac_interface_number: u8,
) -> Vec<u8> {
    let mut collected = Vec::new();
    let mut inside_ac_interface = false;
    let mut pos = 0usize;
    while pos + 1 < raw.len() {
        let length = raw[pos] as usize;
        if length < 2 || pos + length > raw.len() {
            break;
        }
        let descriptor = &raw[pos..pos + length];
        pos += length;
        if descriptor[1] == USB_DT_INTERFACE && length >= 9 {
            inside_ac_interface = descriptor[2] == ac_interface_number
                && descriptor[5] == USB_CLASS_AUDIO
                && descriptor[6] == USB_SUBCLASS_AUDIOCONTROL;
        } else if inside_ac_interface && descriptor[1] == USB_DT_CS_INTERFACE {
            collected.extend_from_slice(descriptor);
        }
    }
    collected
}

#[cfg(test)]
mod implicit_clock_tests {
    use super::*;
    #[test]
    fn implicit_proof_rejects_sparse_sources_and_unknown_multiplier_ratios() {
        let terminal = [8, 0x24, 2, 1, 0, 0, 0, 10];
        let mut descriptors = terminal.to_vec();
        let sparse = ClockGraph::parse(&descriptors)
            .resolve_terminal_clock(1, |_| unreachable!())
            .unwrap();
        assert!(!sparse.proves_identity_cadence());
        descriptors.extend_from_slice(&[8, 0x24, 0x0a, 10, 0, 0, 0, 0]);
        let direct = ClockGraph::parse(&descriptors)
            .resolve_terminal_clock(1, |_| unreachable!())
            .unwrap();
        assert!(direct.proves_identity_cadence());
        descriptors[7] = 11;
        descriptors.extend_from_slice(&[7, 0x24, 0x0c, 11, 10, 0, 0]);
        let multiplied = ClockGraph::parse(&descriptors)
            .resolve_terminal_clock(1, |_| unreachable!())
            .unwrap();
        assert!(!multiplied.proves_identity_cadence());
    }
}

#[cfg(test)]
mod selectable_clock_tests {
    use super::*;

    fn graph(extra: &[u8]) -> ClockGraph {
        let mut bytes = vec![8, 0x24, 2, 1, 0, 0, 0, 10];
        bytes.extend_from_slice(extra);
        ClockGraph::parse(&bytes)
    }

    const SOURCES: &[u8] = &[8, 0x24, 0x0a, 20, 0, 0, 0, 0, 8, 0x24, 0x0a, 21, 0, 0, 0, 0];

    fn selector(controls: u8) -> ClockGraph {
        let mut extra = vec![9, 0x24, 0x0b, 10, 2, 20, 21, controls, 0];
        extra.extend_from_slice(SOURCES);
        graph(&extra)
    }

    #[derive(Default)]
    struct Device {
        pins: HashMap<u8, u8>,
        writes: Vec<(u8, u8)>,
        checked: Vec<u8>,
        invalid_source: bool,
        bad_write: bool,
        fail_restore: bool,
    }

    impl ClockPlanIo for Device {
        fn read_pin(&mut self, id: u8) -> Result<u8, String> {
            self.pins
                .get(&id)
                .copied()
                .ok_or_else(|| "missing pin".into())
        }
        fn write_pin(&mut self, id: u8, pin: u8) -> Result<(), String> {
            self.writes.push((id, pin));
            if self.fail_restore && pin == 1 {
                return Err("restore transfer failed".into());
            }
            if !self.bad_write {
                self.pins.insert(id, pin);
            }
            Ok(())
        }
        fn validate_source(&mut self, id: u8) -> Result<(), String> {
            self.checked.push(id);
            if self.invalid_source {
                Err("source invalid or unsupported rate".into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn valid_current_route_has_priority_and_never_sets() {
        let plans = selector(3).enumerate_plans(1, |_| Ok(2)).unwrap();
        assert_eq!(
            plans.iter().map(|p| p.source_id).collect::<Vec<_>>(),
            [21, 20]
        );
        let mut io = Device {
            pins: HashMap::from([(10, 2)]),
            ..Device::default()
        };
        plans[0].apply(&mut io).unwrap();
        assert!(io.writes.is_empty());
        assert_eq!(io.checked, [21]);
    }

    #[test]
    fn selector_capabilities_and_illegal_pins_block_unsafe_alternatives() {
        assert_eq!(selector(1).enumerate_plans(1, |_| Ok(1)).unwrap().len(), 1);
        for controls in [0, 2] {
            assert!(selector(controls)
                .enumerate_plans(1, |_| panic!("unreadable"))
                .is_err());
        }
        for pin in [0, 3] {
            assert!(selector(3).enumerate_plans(1, |_| Ok(pin)).is_err());
        }
        assert!(selector(3)
            .enumerate_plans(1, |_| Err("GET failed".into()))
            .is_err());
    }

    #[test]
    fn alternatives_are_verified_and_restored_on_failure_or_hand_back() {
        let plan = selector(3).enumerate_plans(1, |_| Ok(1)).unwrap().remove(1);
        let mut io = Device {
            pins: HashMap::from([(10, 1)]),
            invalid_source: true,
            ..Device::default()
        };
        let failure = plan.apply(&mut io).unwrap_err();
        assert!(failure.rollback_errors.is_empty());
        assert_eq!(io.writes, [(10, 2), (10, 1)]);
        io.invalid_source = false;
        plan.apply(&mut io).unwrap();
        plan.restore(&mut io).unwrap();
        assert_eq!(io.pins[&10], 1);
    }

    #[test]
    fn stale_route_and_unverified_set_cannot_validate_an_unrelated_source() {
        let plan = selector(3).enumerate_plans(1, |_| Ok(1)).unwrap().remove(1);
        let mut io = Device {
            pins: HashMap::from([(10, 2)]),
            ..Device::default()
        };
        assert!(plan.apply(&mut io).is_err());
        assert!(io.writes.is_empty());
        io.pins.insert(10, 1);
        io.bad_write = true;
        assert!(plan.apply(&mut io).is_err());
        assert!(io.checked.is_empty());
    }

    #[test]
    fn path_local_cycles_do_not_poison_shared_source_sibling() {
        // First branch loops back to the selector; second reaches a source.
        let mut bytes = vec![9, 0x24, 0x0b, 10, 2, 10, 20, 3, 0];
        bytes.extend_from_slice(SOURCES);
        let plans = graph(&bytes).enumerate_plans(1, |_| Ok(1)).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].source_id, 20);
        // Both sibling branches may reach the same source independently.
        bytes[5] = 20;
        assert_eq!(
            graph(&bytes).enumerate_plans(1, |_| Ok(1)).unwrap().len(),
            2
        );
    }

    #[test]
    fn strict_routes_reject_missing_sources_multipliers_and_truncated_selectors() {
        assert!(graph(&[]).enumerate_plans(1, |_| unreachable!()).is_err());
        let mut bytes = vec![7, 0x24, 0x0c, 10, 20, 0, 0];
        bytes.extend_from_slice(SOURCES);
        assert!(graph(&bytes)
            .enumerate_plans(1, |_| unreachable!())
            .is_err());
        let mut bytes = vec![6, 0x24, 0x0b, 10, 1, 20];
        bytes.extend_from_slice(SOURCES);
        assert!(graph(&bytes)
            .enumerate_plans(1, |_| unreachable!())
            .is_err());
        // Legacy sparse behaviour remains explicitly available.
        assert_eq!(
            graph(&[])
                .resolve_terminal_clock(1, |_| unreachable!())
                .unwrap()
                .clock_id,
            10
        );
    }

    #[test]
    fn failed_rollback_is_visible_and_hand_back_never_overwrites_drift() {
        let plan = selector(3).enumerate_plans(1, |_| Ok(1)).unwrap().remove(1);
        let mut io = Device {
            pins: HashMap::from([(10, 1)]),
            invalid_source: true,
            fail_restore: true,
            ..Device::default()
        };
        let error = plan.apply(&mut io).unwrap_err();
        assert_eq!(error.rollback_errors.len(), 1);
        assert_eq!(io.pins[&10], 2);
        io.writes.clear();
        io.pins.insert(10, 1);
        assert!(plan.restore(&mut io).is_err());
        assert!(io.writes.is_empty());
    }

    #[test]
    fn duplicate_ids_and_excessive_depth_never_produce_selectable_routes() {
        let mut bytes = vec![8, 0x24, 0x0a, 10, 0, 0, 0, 0, 8, 0x24, 0x0a, 10, 0, 0, 0, 0];
        assert!(graph(&bytes)
            .enumerate_plans(1, |_| unreachable!())
            .is_err());
        bytes.clear();
        for id in 10..27 {
            bytes.extend_from_slice(&[8, 0x24, 0x0b, id, 1, id + 1, 0, 0]);
        }
        bytes.extend_from_slice(&[8, 0x24, 0x0a, 27, 0, 0, 0, 0]);
        assert!(graph(&bytes)
            .enumerate_plans(1, |_| unreachable!())
            .is_err());
    }

    #[test]
    fn nested_plan_changes_leaf_first_and_restores_root_first() {
        let mut bytes = vec![
            9, 0x24, 0x0b, 10, 2, 20, 11, 3, 0, 9, 0x24, 0x0b, 11, 2, 20, 21, 3, 0,
        ];
        bytes.extend_from_slice(SOURCES);
        let plan = graph(&bytes)
            .enumerate_plans(1, |_| Ok(1))
            .unwrap()
            .pop()
            .unwrap();
        let mut io = Device {
            pins: HashMap::from([(10, 1), (11, 1)]),
            ..Device::default()
        };
        plan.apply(&mut io).unwrap();
        assert_eq!(io.writes, [(11, 2), (10, 2)]);
        plan.restore(&mut io).unwrap();
        assert_eq!(io.writes, [(11, 2), (10, 2), (10, 1), (11, 1)]);
    }
}
