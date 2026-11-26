use crate::descriptors::error::DescriptorError;

pub trait DescriptorParser {
    type Output;
    fn parse(&self, data: &[u8]) -> Result<Self::Output, DescriptorError>;
}
