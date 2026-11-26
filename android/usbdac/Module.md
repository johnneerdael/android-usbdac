# Module usbdac

Bit-perfect direct USB DAC output for Android.

`UsbDac` finds an attached USB DAC, asks for permission and opens it as a `DacSession`.
A session reports what the DAC can do, negotiates a stream format and opens a `DacStream`
that takes audio as bytes. `AndroidManagedOutput` is the second transport behind the same
`PcmOutput` interface: an `AudioTrack` through Android's own audio stack. `AAudioExclusiveOutput`
is the third: an exclusive AAudio stream, for a device with an internal DAC.

`OutputSelector` chooses between them: it looks at the attached DAC, the USB permission, where Android
sends the sound and what the platform grants, and returns one ready `PcmOutput` (a `SelectedOutput`)
with an `OutputDecision` that says what was chosen, which `Guarantee` applies and why the better
outputs were passed over.

The guide with the worked example is in the repository under `docs/sdk`.
