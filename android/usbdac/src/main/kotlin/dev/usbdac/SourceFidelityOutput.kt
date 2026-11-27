package dev.usbdac

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow

/** A wire-perfect target does not make an altered source bit-perfect. */
internal class SourceFidelityOutput(internal val target: PcmOutput, description: String) : PcmOutput by target {
    override val bitPerfect: StateFlow<BitPerfectState> = MutableStateFlow(BitPerfectState.NotVerified(null, description))
}
