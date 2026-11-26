# Module usbdac

Bit-perfect direct USB DAC output for Android.

`UsbDac` finds an attached USB DAC, asks for permission and opens it as a `DacSession`.
A session reports what the DAC can do, negotiates a stream format and opens a `DacStream`
that takes audio as bytes. `AndroidManagedOutput` is the second transport behind the same
`PcmOutput` interface: an `AudioTrack` through Android's own audio stack.

The guide with the worked example is in the repository under `docs/sdk`.
