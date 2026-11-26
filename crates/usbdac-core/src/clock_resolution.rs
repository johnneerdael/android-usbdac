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
use std::collections::HashMap;
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
}

impl ClockGraph {
    /// Scan class-specific AudioControl descriptors (the interface's `extra`
    /// bytes). Unknown or truncated descriptors are skipped, never fatal.
    pub(crate) fn parse(descriptors: &[u8]) -> Self {
        let mut graph = Self::default();
        let mut index = 0usize;
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
                }
                UAC2_CLOCK_SELECTOR if length >= 4 => {
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
