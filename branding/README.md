# usbdac brand kit

The usbdac identity (visual direction "Duplex", version 1.0). The product name stays
**usbdac** in prose and wordmarks; crates, Maven coordinates, bundle identifiers and
package names are unchanged.

> **Direct USB audio. Clear evidence.**

| Folder | What is in it | Used by |
|---|---|---|
| `logos/` | The mark, horizontal, stacked and descriptor lockups as outlined SVG (dark, light, white, ink), plus 1024 px mark PNGs | README, site, presentations |
| `marketing/` | README banner, Open Graph / social card, GitHub social preview and hero artwork, SVG and PNG, both themes | README, site `og:image`, release notes |
| `web/` | SVG and ICO favicons, touch icon, PWA icons and manifest | The documentation site |
| `fonts/` | Manrope (variable, brand and interface) and IBM Plex Mono (code, device IDs, rates, diagnostics), with their SIL OFL licenses | Site, Android and macOS validators |
| `tokens/` | Semantic colour, type, spacing, radius and motion tokens: CSS, JSON, TypeScript, Kotlin, Swift and Rust | Site stylesheet, validator themes |

Native icon resources live with the apps: `android/validator/src/main/res/` (adaptive,
round and Android 13 themed icons) and `macos/validator/branding/usbdac.icns`.

## Rules that matter

- **Colour.** The mark is Signal Blue `#65A9FF` with Porcelain `#F3F5F7` on graphite, or Deep
  Blue `#2367C9` with Navy `#152237` on light backgrounds. Interface actions use `#78B5FF` on dark
  and `#2367C9` on light. Use the semantic tokens rather than raw hex values.
- **Blue is brand and action, never a verdict.** Verification states (not checked, verified,
  blocked, unsupported, failed, not attempted) are distinct, and colour is always paired with a word.
- **Keep the weave intact.** Do not close the crossing gap, stretch, rotate, bevel or add glow.
  Keep clear space of at least 15% of the mark's size. Minimum sizes: mark 24 px, horizontal
  lockup 150 px; use the favicon tile below that.
- **Type.** Manrope 700–750 for display, 600–700 for controls, 400–500 for body text. IBM Plex
  Mono 400 for code and diagnostic values.
- **Shape and motion.** 4 px grid; 8 px controls, 16 px panels, 24 px sheets; 120 ms feedback,
  180 ms transitions, up to 240 ms sheets, `cubic-bezier(.2,.8,.2,1)`; honour reduced motion.
- **Themes.** Apps offer System / Dark / Light and follow the system until the user chooses.

## Voice

Plain, technical when needed, and specific about evidence. Lead with what the reader can do;
explain the protocol when it helps.

| Situation | Say |
|---|---|
| Product | Direct USB audio for your app. |
| Verified | Verified on this stream. |
| Blocked | Not verified - view the reason. |
| Unknown | Not checked on this device. |
| Report | Report saved, with device details and test outcomes. |

Never present reported capabilities as hardware-tested acceptance, promise that every DAC
works, or turn the logo into a verification badge.
