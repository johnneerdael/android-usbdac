use crate::descriptors::error::DescriptorError;

pub fn require_len(data: &[u8], min: usize) -> Result<(), DescriptorError> {
    if data.len() < min {
        return Err(DescriptorError::Invalid(format!(
            "expected at least {} bytes, got {}",
            min,
            data.len()
        )));
    }
    Ok(())
}

/// Restrict each parser to its own complete declared descriptor, even when
/// the caller supplied a longer configuration or a following descriptor.
pub fn descriptor_bytes(data: &[u8], min: usize) -> Result<&[u8], DescriptorError> {
    require_len(data, min)?;
    let length = usize::from(
        *data
            .first()
            .ok_or_else(|| DescriptorError::Invalid("missing descriptor length".into()))?,
    );
    if length < min {
        return Err(DescriptorError::Invalid(format!(
            "declared descriptor length {length} is smaller than required {min}"
        )));
    }
    checked_slice(data, 0, length)
}

pub fn checked_slice(data: &[u8], offset: usize, length: usize) -> Result<&[u8], DescriptorError> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| DescriptorError::Invalid("descriptor field span overflow".into()))?;
    data.get(offset..end).ok_or_else(|| {
        DescriptorError::Invalid(format!(
            "descriptor field at {offset} needs {length} bytes, available {}",
            data.len()
        ))
    })
}

fn read_field<const N: usize>(data: &[u8], offset: usize) -> Result<[u8; N], DescriptorError> {
    let mut bytes = [0; N];
    bytes.copy_from_slice(checked_slice(data, offset, N)?);
    Ok(bytes)
}

pub fn read_u16_le(data: &[u8], offset: usize) -> Result<u16, DescriptorError> {
    Ok(u16::from_le_bytes(read_field(data, offset)?))
}

pub fn read_u32_le(data: &[u8], offset: usize) -> Result<u32, DescriptorError> {
    Ok(u32::from_le_bytes(read_field(data, offset)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn field_reads_reject_incomplete_or_overflowing_spans() {
        assert!(read_u16_le(&[], 0).is_err());
        assert!(read_u32_le(&[0; 3], 0).is_err());
        assert!(read_u16_le(&[0; 4], usize::MAX).is_err());
        assert!(checked_slice(&[0; 4], 3, 2).is_err());
        assert_eq!(read_u16_le(&[0x34, 0x12], 0).unwrap(), 0x1234);
        assert_eq!(
            read_u32_le(&[0x78, 0x56, 0x34, 0x12], 0).unwrap(),
            0x12345678
        );
    }
}
