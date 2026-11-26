# Product Marketing Context

**Document version:** v1

**Last updated:** 2026-11-26

## Product and audience

usbdac is an MIT-licensed library for developers building audio players.
It gives Android apps direct USB DAC playback with hardware volume control
and stream-level bit-perfect reporting. Its Rust core also serves Android
and Linux developers who manage access to the USB device themselves.

| Developer | First milestone | Dependency |
|---|---|---|
| Android player developer | Add the library, open a PCM output through `OutputSelector` and feed it decoded samples | Android AAR from GitHub Releases |
| Rust audio developer | Pass an open USB file descriptor, negotiate a format and feed the stream samples | Cargo crate `usbdac-core` |

## Positioning

- Lead with the integration task. Name the dependency, the supported platform
  and the next API call.
- For direct USB, explain what the app gains from bypassing the mixer. It can
  send PCM, DoP or native DSD to a compatible DAC, control its hardware volume
  and report what the stream verified.
- For PCM across Android routes, explain that `OutputSelector` returns a ready
  `PcmOutput` with the route, the reason for the choice and its guarantee.
- Explain what the host supplies. It decodes the audio. A Rust host also owns
  USB access and passes the claimed interfaces and file descriptor.

## Language and proof

Use exact terms such as direct USB, Android-managed, AAudio exclusive,
hardware volume and bit-perfect verification. Explain terms through API
behavior and name the relevant method or result. Describe format support and
measured hardware coverage separately. Direct USB PCM has played through
Flick on a OnePlus 15 with Android 16 and an iBasso DC04U. The 0.1 alpha has
limited hardware coverage across devices, formats and Android routes.

## Changelog

- v1 (2026-11-26) — Initial context for Android and Rust developer documentation.
