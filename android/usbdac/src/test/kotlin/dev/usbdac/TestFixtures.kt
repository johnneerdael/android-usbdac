package dev.usbdac

import java.util.concurrent.Executor

/** Runs tasks only when told to, so a test can interleave submissions with a write in flight. */
internal class ManualExecutor : Executor {
    private val tasks = ArrayDeque<Runnable>()
    val pendingTasks: Int get() = tasks.size
    override fun execute(command: Runnable) {
        tasks.addLast(command)
    }

    fun runAll() {
        while (tasks.isNotEmpty()) tasks.removeFirst().run()
    }
}

/** JSON the native library returns, copied from the schemas in docs/API.md section 11. */
internal object Fixtures {
    const val CAPABILITIES = """
    { "uac_version": "v2", "link_speed": "high",
      "pcm_sample_rates": [44100, 48000, 96000], "pcm_bit_depths": [16, 24, 32],
      "channel_counts": [2],
      "supports_pcm": true, "supports_dop": true, "dop_max_dsd_bit_rate": 11289600,
      "supports_native_dsd": false, "native_dsd_bit_rates": [],
      "has_explicit_feedback": true,
      "volume": { "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 },
      "has_hardware_volume": true, "has_mute": true,
      "alt_settings": [ { "interface_number": 1, "alt_setting": 1, "endpoint_address": 1,
          "format": "pcm", "channels": 2, "subslot_bytes": 4, "valid_bits": 32,
          "rate_ranges": [ { "min_hz": 44100, "max_hz": 44100, "step_hz": 0 } ],
          "sample_rates": [44100], "sync_type": "asynchronous", "feedback_endpoint": 129,
          "max_packet_bytes": 1024, "service_interval_us": 125, "uac_version": "v2" } ] }
    """

    const val CAPABILITIES_NO_VOLUME = """
    { "uac_version": "v1", "link_speed": "full",
      "pcm_sample_rates": [44100], "pcm_bit_depths": [16], "channel_counts": [2],
      "supports_pcm": true, "supports_dop": false, "dop_max_dsd_bit_rate": null,
      "supports_native_dsd": true, "native_dsd_bit_rates": [2822400, 5644800],
      "has_explicit_feedback": false, "volume": null,
      "has_hardware_volume": false, "has_mute": false,
      "alt_settings": [ { "interface_number": 1, "alt_setting": 2, "endpoint_address": 2,
          "format": "dsd", "channels": 2, "subslot_bytes": 4, "valid_bits": 32,
          "rate_ranges": [], "sample_rates": [88200], "sync_type": "adaptive",
          "feedback_endpoint": null, "max_packet_bytes": 512, "service_interval_us": 1000,
          "uac_version": "v1" } ] }
    """

    const val STARTUP = """
    { "original_db": [-10.0, -70.0], "applied_db": [-70.0, -70.0], "limit_db": -70.0,
      "attenuated": true, "clamped_to_minimum": false, "max_db": -40.0 }
    """

    const val STARTUP_KEEP = """
    { "original_db": [0.0], "applied_db": [0.0], "limit_db": 0.0,
      "attenuated": false, "clamped_to_minimum": false, "max_db": null }
    """

    const val FORMAT = """
    { "format_id": 7, "encoding": "pcm", "dsd_bit_rate": null, "sample_rate": 44100,
      "channels": 2, "valid_bits": 16, "subslot_bytes": 2, "bytes_per_frame": 4,
      "counter_rate_hz": 44100, "interface_number": 1, "alt_setting": 1,
      "endpoint_address": 1, "sync_type": "asynchronous", "explicit_feedback": true,
      "link_speed": "high", "service_interval_us": 125, "rate_differs_from_request": false }
    """

    const val FORMAT_NATIVE_DSD = """
    { "format_id": 9, "encoding": "dsd_native", "dsd_bit_rate": 2822400, "sample_rate": 88200,
      "channels": 2, "valid_bits": 32, "subslot_bytes": 4, "bytes_per_frame": 8,
      "counter_rate_hz": 352800, "interface_number": 1, "alt_setting": 2,
      "endpoint_address": 2, "sync_type": "adaptive", "explicit_feedback": false,
      "link_speed": "full", "service_interval_us": 1000, "rate_differs_from_request": true }
    """

    /** A stream-statistics document with the optional feedback fields still null. */
    const val STATS = """
    { "state": 1, "bit_perfect": false, "bit_perfect_blocker": "payload_unverified",
      "software_gain": 1.0, "underruns": 2, "lost_frames": 0, "short_packets": 0,
      "zero_packets": 1, "urb_timeouts": 0, "completed_transfers": 4096,
      "frames_written": 441000, "frames_completed": 440000, "played_frames": 430000,
      "latency_frames": 11000, "latency_ms": 249.5, "queued_frames": 9000,
      "buffer_ms": 204, "buffer_target_ms": 250, "buffer_capacity_ms": 500,
      "transfer_queue_depth": 8, "last_turnaround_us": 900, "max_turnaround_us": 2100,
      "last_submit_gap_us": 120, "max_submit_gap_us": 800, "effective_rate_hz": 44100.2,
      "feedback_active": true, "feedback_reports": 55, "feedback_timeouts": 0,
      "feedback_errors": 0, "feedback_rejected": 0, "feedback_age_ms": null,
      "feedback_rate_hz": null, "feedback_stale": false }
    """

    const val VOLUME_STATE = """{ "db": -40.0, "fraction": 0.5, "muted": false }"""
    const val VOLUME_STATE_NO_MUTE = """{ "db": -12.5, "fraction": 0.9, "muted": null }"""
    const val VOLUME_RANGE = """{ "min_db": -127.0, "max_db": 0.0, "res_db": 1.0 }"""
}
