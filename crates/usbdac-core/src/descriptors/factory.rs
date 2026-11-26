use super::audio_control_parser::AudioControlParser;
use super::audio_streaming_parser::AudioStreamingParser;
use super::constants::USB_DT_INTERFACE_ASSOCIATION;
use super::iad_parser::parse_iad_internal;
use super::parser_trait::DescriptorParser;
use super::types::DescriptorKind;
use crate::descriptors::error::DescriptorError;

pub struct DescriptorFactory {
    ac_parser: AudioControlParser,
    as_parser: AudioStreamingParser,
}

impl DescriptorFactory {
    pub fn new() -> Self {
        Self {
            ac_parser: AudioControlParser,
            as_parser: AudioStreamingParser,
        }
    }

    pub fn create(&self, data: &[u8]) -> Result<DescriptorKind, DescriptorError> {
        if data.len() < 2 {
            return Err(DescriptorError::Invalid(
                "descriptor too short".to_string(),
            ));
        }
        match data[1] {
            USB_DT_INTERFACE_ASSOCIATION => parse_iad_internal(data).map(DescriptorKind::Iad),
            d if d == super::constants::USB_DT_CS_INTERFACE && data.len() >= 3 => {
                let subtype = data[2];
                if subtype == super::constants::UAC2_AC_HEADER
                    || subtype == super::constants::UAC2_INPUT_TERMINAL
                    || subtype == super::constants::UAC2_OUTPUT_TERMINAL
                    || subtype == super::constants::UAC2_FEATURE_UNIT
                {
                    self.ac_parser.parse(data).map(DescriptorKind::AudioControl)
                } else if subtype == super::constants::UAC2_AS_GENERAL
                    || subtype == super::constants::UAC2_FORMAT_TYPE
                {
                    self.as_parser
                        .parse(data)
                        .map(DescriptorKind::AudioStreaming)
                } else {
                    Err(DescriptorError::Invalid(format!(
                        "unknown CS descriptor subtype {}",
                        subtype
                    )))
                }
            }
            _ => Err(DescriptorError::Invalid(format!(
                "unknown descriptor type {}",
                data[1]
            ))),
        }
    }
}

impl Default for DescriptorFactory {
    fn default() -> Self {
        Self::new()
    }
}
