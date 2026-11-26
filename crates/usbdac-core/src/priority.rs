//! Best-effort realtime scheduling for the USB output thread.

/// Raises the current thread to `SCHED_FIFO` priority 2, with nice -16 as the
/// fallback when the scheduler policy is denied. Android only; a no-op
/// elsewhere. Returns the `sched_setscheduler` error (the nice fallback has
/// still been applied).
#[cfg(target_os = "android")]
pub(crate) fn raise_audio_priority() -> Result<(), std::io::Error> {
    let param = libc::sched_param { sched_priority: 2 };
    unsafe {
        let result = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        if result != 0 {
            let err = std::io::Error::last_os_error();
            log::warn!(
                "[AudioSched] SCHED_FIFO failed (errno={}): {}",
                err.raw_os_error().unwrap_or(-1),
                err
            );
            libc::setpriority(libc::PRIO_PROCESS, 0, -16);
            log::info!("[AudioSched] set nice=-16");
            return Err(err);
        }
    }
    log::info!("[AudioSched] SCHED_FIFO priority=2 acquired");
    Ok(())
}

#[cfg(not(target_os = "android"))]
pub(crate) fn raise_audio_priority() -> Result<(), std::io::Error> {
    Ok(())
}
