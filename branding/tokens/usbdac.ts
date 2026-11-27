export const usbdac = {
  "name": "usbdac",
  "direction": "Duplex",
  "version": "1.0",
  "date": "2026-11-27",
  "descriptor": "Direct USB audio. Clear evidence.",
  "color": {
    "dark": {
      "canvas": "#11151C",
      "surface": "#1A212C",
      "raised": "#252F3D",
      "text": "#F3F5F7",
      "muted": "#B2C0D2",
      "primary": "#78B5FF",
      "onPrimary": "#101E32",
      "primarySoft": "#213753",
      "secondary": "#CADCF3",
      "border": "#415169",
      "controlBorder": "#7386A1",
      "success": "#87DFC2",
      "successSoft": "#203D34",
      "warning": "#F1CA83",
      "warningSoft": "#413723",
      "danger": "#FFABB8",
      "dangerSoft": "#442C36",
      "focus": "#A3CDFF"
    },
    "light": {
      "canvas": "#F2F5FA",
      "surface": "#FFFFFF",
      "raised": "#E7EDF5",
      "text": "#152237",
      "muted": "#4E6078",
      "primary": "#2367C9",
      "onPrimary": "#FFFFFF",
      "primarySoft": "#E1EDFF",
      "secondary": "#344E70",
      "border": "#ACB9CA",
      "controlBorder": "#6C809B",
      "success": "#17634D",
      "successSoft": "#DDF4EA",
      "warning": "#76500B",
      "warningSoft": "#FFF0D2",
      "danger": "#A02945",
      "dangerSoft": "#FCE5EC",
      "focus": "#1358B7"
    }
  },
  "fonts": {
    "brand": "Manrope",
    "interface": "Manrope",
    "technical": "IBM Plex Mono"
  },
  "spacing": [
    4,
    8,
    12,
    16,
    24,
    32,
    48,
    64,
    96
  ],
  "radii": {
    "control": 8,
    "panel": 16,
    "sheet": 24
  },
  "motion": {
    "feedback": 120,
    "transition": 180,
    "sheet": 240,
    "ease": "cubic-bezier(.2,.8,.2,1)"
  }
} as const;
