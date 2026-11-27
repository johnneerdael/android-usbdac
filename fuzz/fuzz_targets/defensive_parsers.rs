#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| usbdac_core::fuzzing::exercise(data));
