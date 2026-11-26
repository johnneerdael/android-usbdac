use crate::clock_resolution::{
    audio_control_descriptors_from_config, ClockChoice, ClockGraph, ClockHop, ClockHopKind,
    ClockResolution, ClockResolveError, ClockSelection, ResolvedClock,
};

// Descriptor builders. Layouts follow UAC 2.0 sections 4.7.2.1 to 4.7.2.5.

fn input_terminal(id: u8, clock: u8) -> Vec<u8> {
    vec![
        17, 0x24, 0x02, id, 0x01, 0x01, 0, clock, 2, 3, 0, 0, 0, 0, 0, 0, 0,
    ]
}

fn output_terminal(id: u8, source: u8, clock: u8) -> Vec<u8> {
    vec![12, 0x24, 0x03, id, 0x01, 0x03, 0, source, clock, 0, 0, 0]
}

fn clock_source(id: u8) -> Vec<u8> {
    vec![8, 0x24, 0x0a, id, 0x01, 0x07, 0, 0]
}

fn clock_selector(id: u8, pins: &[u8]) -> Vec<u8> {
    let mut descriptor = vec![7 + pins.len() as u8, 0x24, 0x0b, id, pins.len() as u8];
    descriptor.extend_from_slice(pins);
    descriptor.extend_from_slice(&[0x03, 0]);
    descriptor
}

fn clock_multiplier(id: u8, source: u8) -> Vec<u8> {
    vec![7, 0x24, 0x0c, id, source, 0x03, 0]
}

fn descriptors(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn no_request(selector: u8) -> Result<u8, String> {
    panic!("clock selector {selector} must not be queried here");
}

fn resolve(
    bytes: &[u8],
    terminal: u8,
    pin: impl FnMut(u8) -> Result<u8, String>,
) -> Result<ResolvedClock, ClockResolveError> {
    ClockGraph::parse(bytes).resolve_terminal_clock(terminal, pin)
}

#[test]
fn a_terminal_fed_directly_by_a_clock_source_needs_no_request() {
    let bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.clock_id, 4);
    assert_eq!(resolved.path, vec![4]);
    assert!(!resolved.assumed_source, "a described Clock Source is not assumed");
}

#[test]
fn a_selector_with_one_input_needs_no_request() {
    let bytes = descriptors(&[
        input_terminal(1, 6),
        clock_source(4),
        clock_selector(6, &[4]),
    ]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.clock_id, 4);
    assert_eq!(resolved.path, vec![6, 4]);
}

#[test]
fn a_multi_input_selector_follows_the_pin_the_device_reports() {
    // Two clocks. The first in descriptor order is 4, the one feeding the
    // stream is 5 because the selector is on pin 2.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_selector(9, &[4, 5]),
    ]);
    let graph = ClockGraph::parse(&bytes);
    assert_eq!(graph.first_clock_source(), Some(4));

    let mut asked = Vec::new();
    let resolved = graph
        .resolve_terminal_clock(1, |selector| {
            asked.push(selector);
            Ok(2)
        })
        .unwrap();
    assert_eq!(resolved.clock_id, 5);
    assert_eq!(resolved.path, vec![9, 5]);
    assert_eq!(asked, vec![9], "the selector's own ID is what gets queried");

    let first_pin = graph.resolve_terminal_clock(1, |_| Ok(1)).unwrap();
    assert_eq!(first_pin.clock_id, 4);
}

#[test]
fn a_multiplier_has_a_single_input_and_needs_no_request() {
    let bytes = descriptors(&[
        input_terminal(1, 7),
        clock_source(4),
        clock_multiplier(7, 4),
    ]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.clock_id, 4);
    assert_eq!(resolved.path, vec![7, 4]);
}

#[test]
fn a_selector_feeding_a_multiplier_feeding_a_source_resolves_end_to_end() {
    // terminal -> selector 9 [multiplier 7, multiplier 8]; 7 -> source 4,
    // 8 -> source 5. The device has pin 2 selected, so the stream runs on 5.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_multiplier(7, 4),
        clock_multiplier(8, 5),
        clock_selector(9, &[7, 8]),
    ]);
    let resolved = resolve(&bytes, 1, |_| Ok(2)).unwrap();
    assert_eq!(resolved.clock_id, 5);
    assert_eq!(resolved.path, vec![9, 8, 5]);
}

#[test]
fn a_multiplier_feeding_a_selector_resolves_too() {
    let bytes = descriptors(&[
        input_terminal(1, 7),
        clock_source(4),
        clock_source(5),
        clock_selector(6, &[4, 5]),
        clock_multiplier(7, 6),
    ]);
    let resolved = resolve(&bytes, 1, |_| Ok(1)).unwrap();
    assert_eq!(resolved.path, vec![7, 6, 4]);
}

#[test]
fn an_output_terminal_names_its_clock_at_offset_8_not_its_source_at_offset_7() {
    // bSourceID is 9 (a unit, not a clock); bCSourceID is 4.
    let bytes = descriptors(&[output_terminal(2, 9, 4), clock_source(4)]);
    let resolved = resolve(&bytes, 2, no_request).unwrap();
    assert_eq!(resolved.clock_id, 4);
}

#[test]
fn a_loop_is_reported_instead_of_walked_forever() {
    // multiplier 6 -> multiplier 7 -> multiplier 6.
    let bytes = descriptors(&[
        input_terminal(1, 6),
        clock_source(4),
        clock_multiplier(6, 7),
        clock_multiplier(7, 6),
    ]);
    assert_eq!(resolve(&bytes, 1, no_request), Err(ClockResolveError::Loop(6)));

    // A selector whose selected pin points back at itself.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_selector(9, &[4, 9]),
    ]);
    assert_eq!(resolve(&bytes, 1, |_| Ok(2)), Err(ClockResolveError::Loop(9)));
}

#[test]
fn an_endless_chain_is_cut_off() {
    let mut parts = vec![input_terminal(1, 10), clock_source(4)];
    for id in 10..40u8 {
        parts.push(clock_multiplier(id, id + 1));
    }
    assert_eq!(
        resolve(&descriptors(&parts), 1, no_request),
        Err(ClockResolveError::TooDeep)
    );
}

#[test]
fn an_id_with_no_clock_descriptor_is_taken_to_be_the_source() {
    // Sparse descriptors: the terminal names clock 9 but only clock 4 is
    // described. 9 is used rather than guessing that the first clock is meant.
    let bytes = descriptors(&[input_terminal(1, 9), clock_source(4)]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.clock_id, 9);
    assert!(resolved.assumed_source);

    // The same leniency applies part-way down a chain.
    let bytes = descriptors(&[
        input_terminal(1, 7),
        clock_source(4),
        clock_multiplier(7, 12),
    ]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.path, vec![7, 12]);
}

#[test]
fn a_terminal_that_is_not_described_cannot_be_resolved() {
    let bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    assert_eq!(
        resolve(&bytes, 3, no_request),
        Err(ClockResolveError::TerminalNotFound(3))
    );
}

#[test]
fn a_terminal_naming_clock_zero_has_no_clock() {
    let bytes = descriptors(&[input_terminal(1, 0), clock_source(4)]);
    assert_eq!(
        resolve(&bytes, 1, no_request),
        Err(ClockResolveError::TerminalHasNoClock(1))
    );
}

#[test]
fn bad_pins_are_rejected() {
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_selector(9, &[4, 5]),
    ]);
    // Pins are 1-based: 0 is never valid, and 3 is past the last of 2 inputs.
    assert_eq!(
        resolve(&bytes, 1, |_| Ok(0)),
        Err(ClockResolveError::PinOutOfRange {
            selector: 9,
            pin: 0,
            inputs: 2
        })
    );
    assert_eq!(
        resolve(&bytes, 1, |_| Ok(3)),
        Err(ClockResolveError::PinOutOfRange {
            selector: 9,
            pin: 3,
            inputs: 2
        })
    );
    assert_eq!(
        resolve(&bytes, 1, |_| Err("stalled".to_string())),
        Err(ClockResolveError::PinUnavailable {
            selector: 9,
            reason: "stalled".to_string()
        })
    );
}

#[test]
fn a_selector_without_inputs_or_pointing_at_clock_zero_is_malformed() {
    let bytes = descriptors(&[input_terminal(1, 9), clock_source(4), clock_selector(9, &[])]);
    assert_eq!(
        resolve(&bytes, 1, no_request),
        Err(ClockResolveError::SelectorWithoutInputs(9))
    );

    let bytes = descriptors(&[input_terminal(1, 9), clock_source(4), clock_selector(9, &[0, 4])]);
    assert_eq!(
        resolve(&bytes, 1, |_| Ok(1)),
        Err(ClockResolveError::MalformedEntity(9))
    );

    let bytes = descriptors(&[input_terminal(1, 7), clock_source(4), clock_multiplier(7, 0)]);
    assert_eq!(
        resolve(&bytes, 1, no_request),
        Err(ClockResolveError::MalformedEntity(7))
    );
}

#[test]
fn a_truncated_selector_is_malformed_not_a_source() {
    // Claims two pins but ends before the second one.
    let truncated_selector = vec![8, 0x24, 0x0b, 9, 2, 4, 5, 3];
    let bytes = descriptors(&[input_terminal(1, 9), clock_source(4), truncated_selector]);
    assert_eq!(
        resolve(&bytes, 1, no_request),
        Err(ClockResolveError::MalformedEntity(9))
    );
}

#[test]
fn truncated_trailing_descriptors_are_ignored() {
    let mut bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    // A descriptor that claims more bytes than remain.
    bytes.extend_from_slice(&[40, 0x24, 0x0b, 9, 2, 4]);
    let resolved = resolve(&bytes, 1, no_request).unwrap();
    assert_eq!(resolved.clock_id, 4);
}

#[test]
fn descriptors_without_clock_entities_have_nothing_to_resolve() {
    // UAC 1.0 has no clock entities; its terminal descriptors must not be
    // misread as a clock (offset 7 is bNrChannels there).
    let uac1_input_terminal = vec![12, 0x24, 0x02, 1, 0x01, 0x01, 0, 2, 3, 0, 0, 0];
    let graph = ClockGraph::parse(&uac1_input_terminal);
    assert!(!graph.has_clock_entities());
    assert_eq!(graph.first_clock_source(), None);
}

#[test]
fn the_first_clock_source_is_the_first_in_descriptor_order() {
    let bytes = descriptors(&[clock_source(7), clock_selector(9, &[7, 4]), clock_source(4)]);
    assert_eq!(ClockGraph::parse(&bytes).first_clock_source(), Some(7));
}

// A configuration descriptor as the raw GET_DESCRIPTOR fallback sees it.
fn interface(number: u8, class: u8, subclass: u8) -> Vec<u8> {
    vec![9, 0x04, number, 0, 0, class, subclass, 0x20, 0]
}

#[test]
fn the_raw_fallback_collects_only_the_audio_control_interfaces_class_descriptors() {
    let mut raw = vec![9, 0x02, 0, 0, 3, 1, 0, 0x80, 50]; // configuration
    raw.extend(interface(0, 0x01, 0x01)); // AudioControl
    raw.extend(input_terminal(1, 9));
    raw.extend(clock_source(4));
    raw.extend(clock_source(5));
    raw.extend(clock_selector(9, &[4, 5]));
    raw.extend(interface(1, 0x01, 0x02)); // AudioStreaming, must be skipped
    raw.extend(clock_source(77)); // not part of the AC interface
    raw.extend(vec![7, 0x05, 0x01, 0x01, 0, 2, 1]); // endpoint
    raw.extend(interface(2, 0x01, 0x01)); // another AC interface
    raw.extend(clock_source(88));

    let ac0 = audio_control_descriptors_from_config(&raw, 0);
    let graph = ClockGraph::parse(&ac0);
    assert_eq!(graph.first_clock_source(), Some(4));
    let resolved = graph.resolve_terminal_clock(1, |_| Ok(2)).unwrap();
    assert_eq!(resolved.clock_id, 5, "the raw path honours the selector too");

    let ac2 = audio_control_descriptors_from_config(&raw, 2);
    assert_eq!(ClockGraph::parse(&ac2).first_clock_source(), Some(88));

    assert!(audio_control_descriptors_from_config(&raw, 1).is_empty());
}

#[test]
fn the_raw_fallback_survives_a_truncated_configuration() {
    let mut raw = interface(0, 0x01, 0x01);
    raw.extend(clock_source(4));
    raw.extend_from_slice(&[30, 0x24, 0x0a]);
    let collected = audio_control_descriptors_from_config(&raw, 0);
    assert_eq!(ClockGraph::parse(&collected).first_clock_source(), Some(4));
}

// ---- what the library reports about the clock it chose (ClockChoice) ----

/// The AudioControl interface the choice is made on; any number will do.
const AC: u8 = 0;

fn hop(id: u8, kind: ClockHopKind) -> ClockHop {
    ClockHop { id, kind }
}

fn selector_hop(id: u8, selected_pin: u8, pin_count: u8) -> ClockHop {
    hop(
        id,
        ClockHopKind::Selector {
            selected_pin,
            pin_count,
        },
    )
}

fn choose(
    bytes: &[u8],
    terminal_link: Option<u8>,
    pin: impl FnMut(u8) -> Result<u8, String>,
) -> Result<ClockSelection, String> {
    ClockGraph::parse(bytes).select_clock(AC, terminal_link, pin)
}

fn resolved(choice: ClockChoice, source_id: u8, path: Vec<ClockHop>) {
    assert_eq!(choice.control_interface, AC);
    assert_eq!(choice.source_id, source_id);
    assert_eq!(choice.resolution, ClockResolution::Resolved);
    assert_eq!(choice.path, path);
    // The path ends at the Clock Source the rate is set on.
    assert_eq!(choice.path.last(), Some(&hop(source_id, ClockHopKind::Source)));
}

#[test]
fn the_choice_for_a_terminal_fed_directly_by_a_source_is_terminal_then_source() {
    let bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    let selection = choose(&bytes, Some(1), no_request).unwrap();
    assert_eq!(selection.unresolved, None);
    resolved(
        selection.choice,
        4,
        vec![hop(1, ClockHopKind::Terminal), hop(4, ClockHopKind::Source)],
    );
}

#[test]
fn the_choice_through_a_single_input_selector_reports_pin_one_of_one_without_asking() {
    let bytes = descriptors(&[
        input_terminal(1, 6),
        clock_source(4),
        clock_selector(6, &[4]),
    ]);
    let selection = choose(&bytes, Some(1), no_request).unwrap();
    resolved(
        selection.choice,
        4,
        vec![
            hop(1, ClockHopKind::Terminal),
            selector_hop(6, 1, 1),
            hop(4, ClockHopKind::Source),
        ],
    );
}

#[test]
fn the_choice_through_a_multi_input_selector_on_pin_two_names_the_second_source() {
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_selector(9, &[4, 5]),
    ]);
    let selection = choose(&bytes, Some(1), |_| Ok(2)).unwrap();
    resolved(
        selection.choice,
        5,
        vec![
            hop(1, ClockHopKind::Terminal),
            selector_hop(9, 2, 2),
            hop(5, ClockHopKind::Source),
        ],
    );
}

#[test]
fn the_choice_through_a_multiplier_lists_it_between_terminal_and_source() {
    let bytes = descriptors(&[
        input_terminal(1, 7),
        clock_source(4),
        clock_multiplier(7, 4),
    ]);
    let selection = choose(&bytes, Some(1), no_request).unwrap();
    resolved(
        selection.choice,
        4,
        vec![
            hop(1, ClockHopKind::Terminal),
            hop(7, ClockHopKind::Multiplier),
            hop(4, ClockHopKind::Source),
        ],
    );
}

#[test]
fn the_choice_through_a_selector_then_a_multiplier_keeps_the_order_followed() {
    // terminal -> selector 9 [multiplier 7, multiplier 8]; 8 -> source 5 on pin 2.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_multiplier(7, 4),
        clock_multiplier(8, 5),
        clock_selector(9, &[7, 8]),
    ]);
    let selection = choose(&bytes, Some(1), |_| Ok(2)).unwrap();
    resolved(
        selection.choice,
        5,
        vec![
            hop(1, ClockHopKind::Terminal),
            selector_hop(9, 2, 2),
            hop(8, ClockHopKind::Multiplier),
            hop(5, ClockHopKind::Source),
        ],
    );
}

#[test]
fn the_choice_for_an_output_terminal_starts_at_that_terminal() {
    let bytes = descriptors(&[output_terminal(2, 9, 4), clock_source(4)]);
    let selection = choose(&bytes, Some(2), no_request).unwrap();
    resolved(
        selection.choice,
        4,
        vec![hop(2, ClockHopKind::Terminal), hop(4, ClockHopKind::Source)],
    );
}

#[test]
fn an_id_taken_to_be_the_source_is_still_a_resolved_choice() {
    let bytes = descriptors(&[input_terminal(1, 9), clock_source(4)]);
    let selection = choose(&bytes, Some(1), no_request).unwrap();
    assert!(selection.assumed_source);
    resolved(
        selection.choice,
        9,
        vec![hop(1, ClockHopKind::Terminal), hop(9, ClockHopKind::Source)],
    );
}

fn fallback_to_the_first_source(choice: &ClockChoice) {
    assert_eq!(choice.control_interface, AC);
    assert_eq!(choice.source_id, 4, "the first Clock Source in descriptor order");
    assert_eq!(choice.resolution, ClockResolution::FallbackFirstSource);
    // Nothing was followed, so only the source is on the path.
    assert_eq!(choice.path, vec![hop(4, ClockHopKind::Source)]);
}

#[test]
fn an_unresolvable_topology_falls_back_to_the_first_source_and_says_why() {
    // Two clocks behind a selector; the device will not say which pin is active.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_source(4),
        clock_source(5),
        clock_selector(9, &[5, 4]),
    ]);
    let selection = choose(&bytes, Some(1), |_| Err("stalled".to_string())).unwrap();
    fallback_to_the_first_source(&selection.choice);
    let reason = selection.unresolved.expect("a fallback carries its reason");
    assert!(reason.contains("stalled"), "{reason}");

    // A terminal the descriptors do not describe.
    let bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    let selection = choose(&bytes, Some(3), no_request).unwrap();
    fallback_to_the_first_source(&selection.choice);
    assert!(selection.unresolved.is_some());

    // A looping chain.
    let bytes = descriptors(&[
        input_terminal(1, 6),
        clock_source(4),
        clock_multiplier(6, 7),
        clock_multiplier(7, 6),
    ]);
    fallback_to_the_first_source(&choose(&bytes, Some(1), no_request).unwrap().choice);
}

#[test]
fn a_stream_without_a_terminal_link_falls_back_too() {
    let bytes = descriptors(&[input_terminal(1, 4), clock_source(4)]);
    let selection = choose(&bytes, None, no_request).unwrap();
    fallback_to_the_first_source(&selection.choice);
    assert!(selection.unresolved.expect("reason").contains("bTerminalLink"));
}

#[test]
fn with_no_clock_source_at_all_there_is_no_choice() {
    // Only a selector and a multiplier: nothing to fall back on.
    let bytes = descriptors(&[
        input_terminal(1, 9),
        clock_selector(9, &[7]),
        clock_multiplier(7, 0),
    ]);
    assert!(choose(&bytes, Some(1), no_request).is_err());
}

#[test]
fn a_uac1_device_has_no_clock_to_choose() {
    // UAC 1.0 sets the rate on the endpoint and describes no clock entities, so the
    // transport finds no clock and `StreamFormat::clock` is `None`.
    let uac1_input_terminal = vec![12, 0x24, 0x02, 1, 0x01, 0x01, 0, 2, 3, 0, 0, 0];
    let graph = ClockGraph::parse(&uac1_input_terminal);
    assert!(!graph.has_clock_entities());
    assert!(graph.select_clock(AC, Some(1), no_request).is_err());
}

#[test]
fn clock_codes_are_stable() {
    assert_eq!(ClockResolution::Resolved.code(), "resolved");
    assert_eq!(ClockResolution::FallbackFirstSource.code(), "fallback_first_source");
    assert_eq!(ClockHopKind::Terminal.code(), "terminal");
    assert_eq!(
        ClockHopKind::Selector {
            selected_pin: 1,
            pin_count: 2
        }
        .code(),
        "selector"
    );
    assert_eq!(ClockHopKind::Multiplier.code(), "multiplier");
    assert_eq!(ClockHopKind::Source.code(), "source");
}
