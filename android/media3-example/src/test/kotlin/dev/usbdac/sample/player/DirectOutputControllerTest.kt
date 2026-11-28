package dev.usbdac.sample.player

import androidx.media3.common.C
import androidx.media3.common.Format
import androidx.media3.common.MimeTypes
import androidx.media3.exoplayer.audio.AudioSink
import dev.usbdac.*
import dev.usbdac.media3.UsbDacAudioSink
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableSharedFlow
import org.junit.Assert.*
import org.junit.Test
import org.mockito.Mockito
import java.nio.ByteBuffer

class DirectOutputControllerTest {
    @Test fun unplugRemainsDirectAcrossFlushUntilExplicitAndroidSelection() {
        for (unplug in listOf(true, false)) {
        val events = MutableSharedFlow<DacEvent>(extraBufferCapacity = 8)
        val session = Mockito.mock(DacSession::class.java)
        Mockito.`when`(session.events).thenReturn(events)
        fun selected(kind: OutputKind): SelectedOutput {
            val selected = Mockito.mock(SelectedOutput::class.java)
            val decision = Mockito.mock(OutputDecision::class.java)
            Mockito.`when`(decision.kind).thenReturn(kind)
            Mockito.`when`(selected.decision).thenReturn(decision)
            Mockito.`when`(selected.format).thenReturn(WriteFormat(44_100, 2, 2, 16, 44_100))
            Mockito.`when`(selected.writableBytes).thenReturn(4096)
            Mockito.`when`(selected.write(Mockito.any(ByteBuffer::class.java))).thenAnswer {
                val buffer = it.getArgument<ByteBuffer>(0)
                val count = buffer.remaining()
                buffer.position(buffer.limit())
                count
            }
            if (kind == OutputKind.DIRECT_USB) Mockito.`when`(selected.session).thenReturn(session)
            return selected
        }
        val direct = selected(OutputKind.DIRECT_USB)
        val managed = selected(OutputKind.ANDROID_STANDARD)
        var attached = true
        var managedOpens = 0
        var attempts = 0
        val controller = DirectOutputController(DirectOutputSettings(), CoroutineScope(Dispatchers.Unconfined)) { policy ->
            val selector = Mockito.mock(OutputSelector::class.java)
            val snapshot = Mockito.mock(OutputSelector.PcmSupportSnapshot::class.java)
            Mockito.`when`(selector.pcmSupportSnapshot()).thenReturn(snapshot)
            Mockito.`when`(selector.open(Mockito.any(StreamRequest::class.java))).thenAnswer {
                attempts++
                if (attached && policy.allowDirectUsb) direct
                else if (policy.force == OutputKind.DIRECT_USB) throw DacException(DacErrorCode.DEVICE_LOST, "absent")
                else { managedOpens++; managed }
            }
            selector
        }
        val sink = UsbDacAudioSink(controller)
        val format = Format.Builder().setSampleMimeType(MimeTypes.AUDIO_RAW).setPcmEncoding(C.ENCODING_PCM_16BIT)
            .setSampleRate(44_100).setChannelCount(2).build()
        sink.configure(AudioSink.AudioSinkConfig.Builder(format).build())
        sink.play()
        assertTrue(sink.handleBuffer(ByteBuffer.allocateDirect(4), 0, 1))
        attached = false
        val event = if (unplug) DacEvent.DeviceLost else DacEvent.StreamFailed(DacException(DacErrorCode.TRANSFER, "failed"))
        assertTrue(events.tryEmit(event))
        val failure = assertThrows(AudioSink.WriteException::class.java) {
            sink.handleBuffer(ByteBuffer.allocateDirect(4), 0, 1)
        }
        assertEquals(!unplug, failure.isRecoverable)
        sink.flush()
        assertThrows(AudioSink.InitializationException::class.java) {
            sink.handleBuffer(ByteBuffer.allocateDirect(4), 0, 1)
        }
        assertEquals(0, managedOpens)
        val beforeAttach = attempts
        attached = true
        controller.onChanged()
        assertEquals(beforeAttach, attempts)
        attached = false
        controller.apply(DirectOutputSettings(takeOverDac = false))
        sink.flush()
        assertTrue(sink.handleBuffer(ByteBuffer.allocateDirect(4), 0, 1))
        assertEquals(1, managedOpens)
        sink.release()
        }
    }
}
