# USB audio in a page

The [design decisions](decisions.md) assume a few ideas from USB audio. This page explains them in plain terms, with the sources in [the references](references.md). Skip it if you have written a USB audio driver.

## Why a driver goes around Android's audio stack

An Android app normally plays audio through Android's audio framework, which mixes everything that plays at once, may convert it to the rate the output runs at, applies the system volume, and hands the result to the audio HAL and the USB audio driver in the kernel. That is the right design for a phone. For a listener who wants the DAC to receive exactly the numbers in the file, each of those steps is something that can change them.

There are two ways around it. Android 14 added an opt-in "bit-perfect" mixer behaviour for USB devices (`AudioMixerAttributes`), which Android enforces and an app cannot check. The other is what usbdac's direct output does: the app takes the USB device from the kernel driver, becomes the driver, and sends the audio itself. Android's `UsbRequest` class is documented for bulk and interrupt endpoints, and `UsbConstants` lists the isochronous endpoint type as "currently not supported", so this has to go through the device's file descriptor with usbfs and a library such as libusb.

## Isochronous transfers

USB moves data in four ways. Audio uses the **isochronous** kind. An isochronous endpoint is promised a slice of bandwidth in every frame, and in exchange nothing is retried: the USB 2.0 specification says "in the case of a delivery failure due to error, no retrying of the attempt to deliver the data". A packet that does not arrive is gone.

Time on the bus is cut into **frames** of 1 ms (full speed) or **microframes** of 125 microseconds (high speed). A DAC's audio streaming endpoint is serviced every so many of those, its **service interval**: every 125 us, every 250 us, and so on up to 1 ms or more. The host sends one **packet** per service interval, and the packet carries however many audio frames belong to that slice of time.

That rarely divides evenly. At 44.1 kHz and 125 us, a packet should carry 5.5125 audio frames. The driver sends 5 frames in some packets and 6 in others, and carries the remainder forward, so that after a second it has sent exactly 44,100.

Software does not send packets one at a time. It builds a **transfer**, a batch of packets handed to the host controller together, and keeps several transfers **in flight** so the controller always has the next batch. The number of transfers and their size decide how much audio is queued ahead of the DAC, and that is [the first design decision](decisions-stream.md#queue-geometry).

## The DAC's clock and the host's clock

A DAC has its own oscillator. In **asynchronous** mode, which many hi-fi DACs use, that oscillator is the reference, and the phone has to follow it. The DAC tells the host how many samples it needs per (micro)frame on a second, small endpoint, the **feedback endpoint**. The value is a fixed-point number: 10.14 (ten integer bits, fourteen fractional) on a full-speed link and 16.16 on a high-speed one, according to the USB 2.0 specification. On a SuperSpeed link an eight-byte report carries a 32.32 count of samples per 125 microsecond bus interval. The host reads it and adjusts how many samples go into the following packets ([decision 7](decisions-clock.md#feedback)).

Some asynchronous devices communicate timing through the number of frames in
each packet of an IN audio stream. This is **implicit feedback**. usbdac can
use it when an input stream of the same audio function (UAC2, or UAC3 with a
marked input) shares the playback clock, wire rate and USB service interval.
The driver uses packet lengths for timing. Apps continue to supply the
playback samples. [Implicit feedback](decisions-clock.md#implicit-feedback)

In **synchronous** mode the DAC derives its clock from the bus. In **adaptive**
mode it follows the host's packet sizes.

## Describing the DAC: descriptors, alternate settings, entities

A USB device describes itself with **descriptors**. A USB Audio Class device has an **AudioControl** interface and one or more **AudioStreaming** interfaces. An AudioStreaming interface has several **alternate settings**: alternate 0 has no endpoints (the stream is off), and each other one is a combination of channels, bit depth, slot size and rates, with the endpoint that carries it.

The AudioControl interface describes a small graph of **entities**: an input terminal where the USB stream enters, a **Feature Unit** that holds the volume and mute controls, an output terminal, and, in USB Audio Class 2.0, **clock entities**. A **Clock Source** produces a sample rate. A **Clock Selector** chooses between several clocks. A **Clock Multiplier** derives one clock from another. The terminal says which clock entity drives it, and the sample rate has to be set on the Clock Source at the end of that chain ([decision 6](decisions-clock.md#clock-path)). USB Audio Class 1.0 has no clock entities and sets the rate on the endpoint. USB Audio Class 3.0 keeps the same idea with its own descriptors, adds power domains that must be awake before a stream runs, and defines BADD, a small profile with a fixed 48 kHz format and a topology the driver infers from the endpoints.

The slot a sample occupies on the wire may be larger than the sample: 24-bit audio is often carried in 3 bytes, and sometimes in 4, left-justified. The [native API document](native-api.html#4-sample-layouts) tables the layouts.

## Volume

A Feature Unit's volume is a 16-bit signed number in steps of 1/256 dB, with a range the DAC reports (a minimum, a maximum, a resolution). The host sets it with a control request on the control endpoint, which is a different pipe from the audio. Volume done this way happens inside the DAC, after the samples have arrived, so the audio the DAC receives is untouched ([decision 9](decisions-volume.md#volume-ownership)).

## DSD

DSD is a one-bit stream at a very high rate (2.8224 MHz for DSD64). It can reach a DAC two ways. **DoP** (DSD over PCM) packs 16 DSD bits into the low 16 bits of a 24-bit PCM sample and uses the top byte as a marker that alternates between `0x05` and `0xFA`, so a DAC can recognise the stream; DSD64 travels as 24-bit PCM at 176.4 kHz. **Native DSD** on UAC3 is a standard format with 64-bit words. On older classes it uses a DSD alternate setting whose byte order and slot size depend on the DAC, and the Linux kernel keeps a list of devices for this reason ([decision 5](decisions-stream.md#dsd)).

## What "bit-perfect" can and cannot mean

Bit-perfect means that the numbers in the USB packets are the numbers in the file. It says nothing about what the DAC does next: its own filters, its oversampling, its volume. An app that has changed the samples before handing them over (software volume, an equaliser, a resampler, a float-to-integer conversion) cannot claim it, and the library cannot see that. What the library can check, and what it names when it fails, is on [the decision about the claim](decisions-reporting.md#bit-perfect-claim).
