package dev.usbdac.controls

import android.media.AudioManager
import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ControlsLogicTest {
    // ---- volume keys ----

    @Test
    fun aVolumeKeyDownStepsTheDacInItsDirection() {
        assertEquals(KeyDecision(1), volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.ACTION_DOWN, true))
        assertEquals(KeyDecision(-1), volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.ACTION_DOWN, true))
    }

    @Test
    fun theReleaseIsConsumedWithoutAStep() {
        assertEquals(KeyDecision(0), volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.ACTION_UP, true))
        assertEquals(KeyDecision(0), volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.ACTION_UP, true))
    }

    @Test
    fun otherKeysAreLeftToTheSystem() {
        assertNull(volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_MUTE, KeyEvent.ACTION_DOWN, true))
        assertNull(volumeKeyDecision(KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE, KeyEvent.ACTION_DOWN, true))
        assertNull(volumeKeyDecision(KeyEvent.KEYCODE_BACK, KeyEvent.ACTION_UP, true))
    }

    @Test
    fun withoutADacVolumeEveryKeyIsLeftToTheSystem() {
        assertNull(volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_UP, KeyEvent.ACTION_DOWN, false))
        assertNull(volumeKeyDecision(KeyEvent.KEYCODE_VOLUME_DOWN, KeyEvent.ACTION_UP, false))
    }

    // ---- remote volume provider ----

    @Test
    fun theSystemPanelAndTheDacShareAHundredStepScale() {
        assertEquals(0, fractionToIndex(0.0))
        assertEquals(50, fractionToIndex(0.5))
        assertEquals(100, fractionToIndex(1.0))
        assertEquals(37, fractionToIndex(0.372))
        assertEquals(0.0, indexToFraction(0), 0.0)
        assertEquals(0.5, indexToFraction(50), 0.0)
        assertEquals(1.0, indexToFraction(100), 0.0)
    }

    @Test
    fun outOfRangeAndNanAreKeptInsideTheScale() {
        assertEquals(100, fractionToIndex(7.0))
        assertEquals(0, fractionToIndex(-1.0))
        assertEquals(0, fractionToIndex(Double.NaN))
        assertEquals(1.0, indexToFraction(250), 0.0)
        assertEquals(0.0, indexToFraction(-5), 0.0)
    }

    @Test
    fun raiseAndLowerStepAndSameDoesNothing() {
        assertEquals(RemoteVolumeAction.Step(1), remoteVolumeAction(AudioManager.ADJUST_RAISE, true, false))
        assertEquals(RemoteVolumeAction.Step(-1), remoteVolumeAction(AudioManager.ADJUST_LOWER, false, null))
        assertNull(remoteVolumeAction(AudioManager.ADJUST_SAME, true, false))
    }

    @Test
    fun muteRequestsNeedADacWithMute() {
        assertEquals(RemoteVolumeAction.Mute(true), remoteVolumeAction(AudioManager.ADJUST_MUTE, true, false))
        assertEquals(RemoteVolumeAction.Mute(false), remoteVolumeAction(AudioManager.ADJUST_UNMUTE, true, true))
        assertNull(remoteVolumeAction(AudioManager.ADJUST_MUTE, false, null))
        assertNull(remoteVolumeAction(AudioManager.ADJUST_TOGGLE_MUTE, false, null))
    }

    @Test
    fun toggleFlipsTheCurrentMute() {
        assertEquals(RemoteVolumeAction.Mute(true), remoteVolumeAction(AudioManager.ADJUST_TOGGLE_MUTE, true, false))
        assertEquals(RemoteVolumeAction.Mute(false), remoteVolumeAction(AudioManager.ADJUST_TOGGLE_MUTE, true, true))
        assertEquals(RemoteVolumeAction.Mute(true), remoteVolumeAction(AudioManager.ADJUST_TOGGLE_MUTE, true, null))
    }
}
