//! Bounded defensive fuzz adapters; absent from normal production builds.
use crate::clock_resolution::ClockGraph;
use crate::descriptors::{self as d, uac3};

/// Exercise untrusted descriptors, clock walks, feedback and control replies.
/// Every input is bounded to 4096 bytes; no USB or hardware writes occur.
pub fn exercise(input: &[u8]) {
    let input = &input[..input.len().min(4096)];
    parse_descriptor(input);
    for descriptor in d::DescriptorIter::new(input) {
        parse_descriptor(descriptor);
    }
    let graph = ClockGraph::parse(input);
    let adc3 = ClockGraph::parse_adc3(input);
    for terminal in input.iter().take(8).copied().chain(std::iter::once(1)) {
        let selected = input.last().copied().unwrap_or(1);
        let _ = graph.resolve_terminal_clock(terminal, |_| Ok(selected));
        let _ = graph.enumerate_plans(terminal, |_| Ok(selected));
        if let Ok(graph) = &adc3 {
            let _ = graph.resolve_terminal_clock(terminal, |_| Ok(selected));
            let _ = graph.enumerate_plans(terminal, |_| Ok(selected));
        }
    }
    crate::transport::fuzz_transport_bytes(input);
    crate::scarlett::fuzz_reply(input);
}
fn parse_descriptor(data: &[u8]) {
    macro_rules! parse { ($($parser:path),+ $(,)?) => { $(let _ = $parser(data);)+ }; }
    parse!(
        d::parse_ac_descriptor,
        d::parse_as_descriptor,
        d::parse_iad,
        d::parse_ac_interface_header,
        d::parse_ac_interface_header_v1,
        d::parse_input_terminal,
        d::parse_output_terminal,
        d::parse_feature_unit,
        d::parse_clock_source,
        d::parse_clock_selector,
        d::parse_clock_multiplier,
        d::parse_selector_unit,
        d::parse_mixer_unit,
        d::parse_processing_unit,
        d::parse_extension_unit,
        d::parse_effect_unit,
        d::parse_format_type_i,
        d::parse_format_type_ii,
        d::parse_format_type_iii,
        d::parse_endpoint_specific,
        uac3::parse_ac_header,
        uac3::parse_as_header,
        uac3::parse_input_terminal,
        uac3::parse_output_terminal,
        uac3::parse_clock_source,
        uac3::parse_clock_selector,
        uac3::parse_clock_multiplier,
        uac3::parse_power_domain,
        uac3::parse_feature_unit
    );
    if let Ok(header) = uac3::parse_cluster_header(data, 1, 4096) {
        let _ = uac3::parse_cluster(data, header);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn defensive_corpus_includes_truncated_lengths_loops_and_control_replies() {
        for input in [
            include_bytes!("../../../fuzz/corpus/defensive_parsers/truncated").as_slice(),
            include_bytes!("../../../fuzz/corpus/defensive_parsers/uac2-clock-loop").as_slice(),
            include_bytes!("../../../fuzz/corpus/defensive_parsers/uac3-control").as_slice(),
            include_bytes!("../../../fuzz/corpus/defensive_parsers/feedback-q16").as_slice(),
        ] {
            super::exercise(input);
        }
        // Deterministic malformed-byte sweeps exercise every descriptor subtype
        // without a random seed or an unbounded CI runtime.
        for byte in 0..=255u8 {
            super::exercise(&[byte; 64]);
        }
    }
}
