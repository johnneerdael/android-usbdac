//! Crate-internal logging macros.

/// Verbose transport diagnostics. Goes to `log::debug!`, so a host that wants
/// the full trace enables `usbdac_core=debug`; the notable events use
/// `info!`/`warn!`/`error!` directly.
macro_rules! dev_eprintln {
    ($($arg:tt)*) => {
        log::debug!($($arg)*)
    };
}
