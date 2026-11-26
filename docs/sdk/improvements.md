# What usbdac improves

usbdac began as the USB DAC driver inside the Flick music player. It has since been reworked far enough that it is a different piece of software: the driver was taken apart, given a public API, and changed in the ways listed here. Flick's own driver does not have these changes.

Each entry says what changed, then what that means **in plain terms**.

Several of these came from studying how two other open-source players, Mornye and BitChord, behave with the same DACs. No code from either was used.

Nothing on this page has been confirmed on hardware yet. Every item is covered by automated tests; see [the status](index.md#status).

## Sound that does not drop out

### The right clock is always the one that gets set

A DAC's sample rate is set on a "clock" inside it. Some DACs have more than one clock, with a switch (a clock selector) or a multiplier in between. The original driver only understood the simple layout and otherwise guessed the first clock it found. usbdac follows the selector and multiplier chain to the clock that is actually feeding the output, and reports the path it took.

**In plain terms:** on a DAC with two clocks, for example one for USB and one for a digital input, the old code could set the speed on the wrong one. The music would play at the wrong pitch or not at all. usbdac asks the DAC which clock is in use and sets that one.

### A fixed, small amount of audio waiting on the cable

Audio travels to the DAC in small packets, and the driver keeps a short queue of them lined up on the USB connection. usbdac always queues 16 milliseconds, whatever the DAC's packet size and for PCM, DoP and native DSD alike, and can never queue 256 packets at once. The original queued up to 128 ms on some DACs and exactly 256 packets for DSD.

**In plain terms:** on one tested phone, having exactly 256 packets lined up made the sound stop after two seconds. usbdac is built so that number cannot be reached. And because the queue is the same short length on every DAC, pausing and seeking feel equally quick on all of them.

### A hiccup on the cable no longer ends playback

When a batch of packets is not confirmed in time, usbdac sends the next batch and counts exactly how much audio was affected, up to a limit. The original stopped the stream on the first such event, and also rejected some packet sizes that the USB standard allows.

**In plain terms:** a brief stumble between phone and DAC used to stop the music. Now playback carries on, and the library remembers that a sliver of audio was lost, so it will not claim the playback was perfect.

### A deeper reserve of audio, without slower controls

usbdac keeps a quarter of a second of audio ready (the original kept a tenth). The reserve shrinks back automatically while the app is applying software volume, so that change is still heard promptly.

**In plain terms:** if the phone is briefly busy, or a file is slow to read, there is more music in hand before you would hear a gap. The next section explains why this larger reserve does not make the controls feel slow.

### DACs it cannot pace properly are refused, not played badly

A DAC and a phone each keep their own time, and the DAC must tell the phone how fast to send. Most hi-fi DACs do this through a dedicated return channel ("explicit feedback"), which usbdac supports. A few devices, mostly recording interfaces, expect the phone to work it out from their recording input ("implicit feedback"), which usbdac does not support. Those are refused with a clear error so the app can use Android's own output instead. An app can override this and accept the consequences.

**In plain terms:** rather than play such a device with a click every few minutes as the two clocks drift apart, usbdac says "I can't drive this one properly" and lets the app fall back.

## Controls that respond at once

### Pause stops the music now, and resume loses nothing

On pause, usbdac immediately sends silence to the DAC and keeps the audio it was holding. On resume it continues from exactly that audio. The original kept playing whatever was already buffered before going silent, and started again with a moment of silence.

**In plain terms:** press pause and the sound stops, instead of running on for a tenth of a second. Press play and it picks up at the same sample. The DAC stays "awake" on silence during the pause, so it does not click or need to re-lock.

### Seeking is heard immediately

A flush (used for seek, stop and skip) discards the held audio and also withdraws the packets already lined up on the cable.

**In plain terms:** when you drag to another part of a song, you hear the new position within a couple of milliseconds, with no leftover snippet of the old position first.

### Software volume takes effect within milliseconds

When an app uses software volume (for a DAC with no volume control of its own), usbdac applies it at the last moment before the audio leaves, not when it enters the reserve.

**In plain terms:** the volume changes as you move the slider, not a quarter of a second later.

### The position shown is what you are hearing

usbdac counts the audio the DAC has confirmed receiving, leaving out silence and anything lost, and reports how much is still on its way.

**In plain terms:** the progress bar and synced lyrics match what is coming out of the headphones, rather than running slightly ahead.

## Volume that is safe and behaves

### The first sound is never at full blast

When a DAC is opened, usbdac lowers its hardware volume to at most −40 dB before any audio can play, never raises it, and checks that the DAC obeyed. If the DAC has no usable volume control, usbdac refuses to play unless the app has explicitly allowed fixed-level output.

**In plain terms:** plugging in a DAC that was left at maximum cannot blast your ears or your headphones. And an app has to make a deliberate choice before playing through a DAC whose loudness nothing can turn down.

### The DAC's own volume control, done properly

- The volume slider maps onto the useful range. Many DACs report a range that goes far below anything audible; usbdac spreads the slider over the top 80 dB and keeps zero as silence.
- While a slider is dragged, only the newest position is sent, one at a time, and the last one always arrives.
- Volume and mute work even when no music is playing.
- Ready-made helpers let the phone's volume buttons and Android's volume panel control the DAC.

**In plain terms:** the slider is not dead for its bottom third, dragging it feels smooth and lands where you let go, you can set the level before pressing play, and the phone's volume buttons do what you expect, even with the screen off.

Hardware volume also matters for quality: it turns the level down inside the DAC, so the audio sent to it stays untouched.

### The DAC is handed back the way it was found

usbdac remembers the level the DAC had before it changed anything and puts it back when the app lets go of the DAC.

**In plain terms:** after you close the music app, other apps and system sounds play at the level the DAC had before, not at whatever the music app last set.

### The DAC can rest during a long pause

A helper closes the audio stream after 30 seconds of pause while keeping hold of the DAC, and reopens it on play.

**In plain terms:** a paused player stops streaming silence, so the phone and a bus-powered DAC use less battery. A short pause still resumes instantly.

## An honest "bit-perfect" indicator

"Bit-perfect" means the DAC receives exactly the numbers that are in the music file, with nothing resampled, rounded or scaled on the way.

usbdac reports this as a fact it checks, not a promise. The flag is true only when the DAC's clock was confirmed at the file's sample rate, the outgoing data passed its own inspection, no audio was lost, and no software volume or conversion is active. Whenever it is false, the library names the reason.

**In plain terms:** an app can show a "bit-perfect" light that actually goes out when something compromises the sound, and can tell the listener why.

This is also what makes MQA work without any MQA code: an MQA file is ordinary audio with a hidden signal in its finest detail. If the data arrives untouched, an MQA-capable DAC recognises it and does the rest itself.

### Diagnostics a developer can act on

For each stream, usbdac reports which clock it chose and the path it followed to get there, and how the stream is being kept in time with the DAC (explicit feedback, adaptive, synchronous, or unsynchronised).

**In plain terms:** when a user reports "wrong pitch on my DAC", the developer can see what the library decided without needing USB test equipment.

## Built to be used by any app

### A library, not a part of one player

The original driver was woven into one app's audio engine. usbdac stands alone: an app opens a DAC, asks what it supports, and writes audio into it. Every setting has a documented default, errors have stable codes, and events (errors, lost audio, volume changes, unplugging) arrive on their own thread so they can never interrupt the sound.

**In plain terms:** a developer adds one dependency and writes a few dozen lines, instead of writing a USB audio driver.

### Three ways out, one way to use them

Direct USB, an Android-managed output (which asks Android 14 and later for its own bit-perfect mode), and an AAudio exclusive output for players with a built-in DAC all share one interface. The last two never claim to be verified bit-perfect, because an app cannot see past Android's audio system; they report what they asked for and what they were given.

**In plain terms:** the same app code can serve a phone with a USB dongle and a dedicated music player with an internal DAC, and it tells the truth about which guarantees apply to each.

### Optional conversion, clearly labelled

If a DAC cannot play a file's sample rate or bit depth, an app may ask usbdac to convert: mono to stereo, a different bit depth, or a different sample rate using a high-quality resampler (measured at about 110 dB of rejection). Every conversion is classified as exact, lossless or lossy, and anything other than exact turns the bit-perfect flag off.

**In plain terms:** the file still plays on a limited DAC, and the app is told plainly that it is no longer bit-perfect. Nothing is ever converted unless the app asks.

### Around the driver

A Kotlin API for Android, a sample app that the guide's code is taken from, automated tests for the driver and the Kotlin layer, and published builds for four processor types.

## What it does not do yet

- **Implicit feedback** is not supported; such devices are refused (see above).
- **One DAC at a time** per app.
- **Playback only**, for USB Audio Class 1 and 2 devices.
- **Not yet confirmed on hardware.** A build of Flick that runs on this library exists for that purpose.

## Where it came from

The starting point was Flick's USB driver (MIT licensed, © Flick Player Contributors), including its descriptor parsing, DSD support and hardware-tested quirks. That origin is recorded in the repository's `NOTICE` file.
