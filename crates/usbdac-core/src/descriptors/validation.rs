use super::types::*;
use crate::descriptors::error::DescriptorError;

pub fn validate_iad(iad: &Iad) -> Result<(), DescriptorError> {
    if iad.b_interface_count == 0 {
        return Err(DescriptorError::Invalid(
            "b_interface_count must be > 0".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_ac_interface_header(h: &AcInterfaceHeader) -> Result<(), DescriptorError> {
    if h.w_total_length == 0 {
        return Err(DescriptorError::Invalid(
            "w_total_length must be > 0".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_input_terminal(t: &InputTerminal) -> Result<(), DescriptorError> {
    if t.b_terminal_id == 0 {
        return Err(DescriptorError::Invalid(
            "b_terminal_id must be non-zero".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_output_terminal(t: &OutputTerminal) -> Result<(), DescriptorError> {
    if t.b_terminal_id == 0 {
        return Err(DescriptorError::Invalid(
            "b_terminal_id must be non-zero".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_feature_unit(f: &FeatureUnit) -> Result<(), DescriptorError> {
    if f.b_unit_id == 0 {
        return Err(DescriptorError::Invalid(
            "b_unit_id must be non-zero".to_string(),
        ));
    }
    if !(1..=4).contains(&f.b_control_size) || f.bma_controls.is_empty() {
        return Err(DescriptorError::Invalid(
            "feature unit must have 1..=4 byte controls and a master entry".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_as_interface_general(g: &AsInterfaceGeneral) -> Result<(), DescriptorError> {
    if g.w_format_tag == 0 {
        return Err(DescriptorError::Invalid(
            "w_format_tag must be non-zero".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_format_type_i(f: &FormatTypeI) -> Result<(), DescriptorError> {
    if f.b_subslot_size == 0 || f.b_bit_resolution == 0 {
        return Err(DescriptorError::Invalid(
            "b_subslot_size and b_bit_resolution must be non-zero".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_format_type_ii(f: &FormatTypeII) -> Result<(), DescriptorError> {
    if f.w_max_bit_rate == 0 || f.w_samples_per_frame == 0 {
        return Err(DescriptorError::Invalid(
            "w_max_bit_rate and w_samples_per_frame must be non-zero".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_format_type_iii(f: &FormatTypeIII) -> Result<(), DescriptorError> {
    if f.b_subslot_size == 0 || f.b_bit_resolution == 0 {
        return Err(DescriptorError::Invalid(
            "b_subslot_size and b_bit_resolution must be non-zero".to_string(),
        ));
    }
    Ok(())
}
