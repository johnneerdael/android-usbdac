//! Explicit Pioneer DJM routing choices, independent of the USB backend.
//!
//! Values and option order follow Linux ALSA `sound/usb/mixer_quirks.c`
//! (`snd_djm_opts_*` and `snd_djm_get_label_*`). These tables describe writes;
//! they do not report mixer state or select defaults on connection.

/// DJM models with both source-proven routing requests and PCM transport support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DjmModel {
    Djm250Mk2,
    Djm450,
    Djm750,
    Djm850,
    Djm900Nxs2,
    Djm750Mk2,
    DjmV10,
}

/// A routing control. Pair numbers are one-based USB stereo pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DjmControlKey {
    CaptureLevel,
    CaptureSource(u8),
    PlaybackDestination(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DjmOption {
    /// Zero-based option ordinal, scoped to this control and model.
    pub id: u8,
    pub label: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DjmControl {
    pub key: DjmControlKey,
    pub options: Vec<DjmOption>,
}

/// Available choices for a proven model in an active USB configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DjmRoutingProfile {
    pub model: DjmModel,
    pub configuration: u8,
    pub controls: Vec<DjmControl>,
}

// The upper byte is the USB stereo pair. Share only identical lower-byte
// sequences; their ordering is the ALSA enumeration ordering.
const TWO_CHANNEL: &[u8] = &[0x03, 0x00, 0x06, 0x07, 0x08, 0x09, 0x0d, 0x0a];
const TWO_MASTER: &[u8] = &[0x0a, 0x11, 0x12, 0x07, 0x08, 0x09, 0x0d];
const CD_PHONO: &[u8] = &[0x01, 0x03, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0f];
const LINE_CD: &[u8] = &[0x00, 0x01, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0f];
const LINE_PHONO: &[u8] = &[0x00, 0x03, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0f];
const DIGITAL_CHANNEL: &[u8] = &[0x00, 0x02, 0x03, 0x06, 0x07, 0x08, 0x09, 0x0a];
const FOUR_MASTER: &[u8] = &[0x07, 0x08, 0x09, 0x0a, 0x11, 0x12, 0x13, 0x14];
const V10_PHONO: &[u8] = &[
    0x03, 0x00, 0x02, 0x06, 0x10, 0x07, 0x08, 0x09, 0x0a, 0x21, 0x22,
];
const V10_LINE: &[u8] = &[0x00, 0x02, 0x06, 0x10, 0x07, 0x08, 0x09, 0x0a, 0x21, 0x22];
const PLAYBACK: &[u8] = &[0x00, 0x01, 0x04];
const LEVEL_COMMON: &[&str] = &["-19dB", "-15dB", "-10dB", "-5dB"];
const LEVEL_V10: &[&str] = &["+15dB", "+12dB", "+9dB", "+6dB", "+3dB", "0dB"];

impl DjmModel {
    pub fn key(self) -> &'static str {
        match self {
            Self::Djm250Mk2 => "djm-250mk2",
            Self::Djm450 => "djm-450",
            Self::Djm750 => "djm-750",
            Self::Djm850 => "djm-850",
            Self::Djm900Nxs2 => "djm-900nxs2",
            Self::Djm750Mk2 => "djm-750mk2",
            Self::DjmV10 => "djm-v10",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "djm-250mk2" => Some(Self::Djm250Mk2),
            "djm-450" => Some(Self::Djm450),
            "djm-750" => Some(Self::Djm750),
            "djm-850" => Some(Self::Djm850),
            "djm-900nxs2" => Some(Self::Djm900Nxs2),
            "djm-750mk2" => Some(Self::Djm750Mk2),
            "djm-v10" => Some(Self::DjmV10),
            _ => None,
        }
    }

    pub(crate) fn from_usb_id(vendor: u16, product: u16) -> Option<Self> {
        match (vendor, product) {
            (0x2b73, 0x0017) => Some(Self::Djm250Mk2),
            (0x2b73, 0x0013) => Some(Self::Djm450),
            (0x08e4, 0x017f) => Some(Self::Djm750),
            (0x08e4, 0x0163) => Some(Self::Djm850),
            (0x2b73, 0x000a) => Some(Self::Djm900Nxs2),
            (0x2b73, 0x001b) => Some(Self::Djm750Mk2),
            (0x2b73, 0x0034) => Some(Self::DjmV10),
            _ => None,
        }
    }

    fn levels(self) -> &'static [&'static str] {
        if self == Self::DjmV10 {
            LEVEL_V10
        } else {
            LEVEL_COMMON
        }
    }

    fn capture_sources(self, pair: u8) -> Option<&'static [u8]> {
        match (self, pair) {
            (Self::Djm250Mk2 | Self::Djm450, 1..=2) => Some(TWO_CHANNEL),
            (Self::Djm250Mk2 | Self::Djm450, 3) => Some(TWO_MASTER),
            (Self::Djm750, 1 | 4) => Some(CD_PHONO),
            (Self::Djm750 | Self::Djm850, 2..=3) => Some(LINE_CD),
            (Self::Djm850, 1 | 4) => Some(LINE_PHONO),
            (Self::Djm900Nxs2 | Self::Djm750Mk2, 1..=4) => Some(DIGITAL_CHANNEL),
            (Self::Djm900Nxs2 | Self::Djm750Mk2, 5) => Some(FOUR_MASTER),
            (Self::DjmV10, 1 | 3 | 4 | 6) => Some(V10_PHONO),
            (Self::DjmV10, 2 | 5) => Some(V10_LINE),
            _ => None,
        }
    }

    fn has_playback(self) -> bool {
        matches!(self, Self::Djm250Mk2 | Self::Djm450 | Self::Djm750Mk2)
    }

    pub fn controls(self) -> Vec<DjmControl> {
        let mut controls = vec![DjmControl {
            key: DjmControlKey::CaptureLevel,
            options: self
                .levels()
                .iter()
                .enumerate()
                .map(|(id, &label)| DjmOption {
                    id: id as u8,
                    label,
                })
                .collect(),
        }];
        for pair in 1..=6 {
            if let Some(sources) = self.capture_sources(pair) {
                controls.push(DjmControl {
                    key: DjmControlKey::CaptureSource(pair),
                    options: sources
                        .iter()
                        .enumerate()
                        .map(|(id, &value)| DjmOption {
                            id: id as u8,
                            label: self.capture_label(value),
                        })
                        .collect(),
                });
            }
        }
        if self.has_playback() {
            for pair in 1..=3 {
                controls.push(DjmControl {
                    key: DjmControlKey::PlaybackDestination(pair),
                    options: ["Ch1", "Ch2", "Aux"]
                        .into_iter()
                        .enumerate()
                        .map(|(id, label)| DjmOption {
                            id: id as u8,
                            label,
                        })
                        .collect(),
                });
            }
        }
        controls
    }

    /// Resolve an explicitly selected choice to `(wValue, wIndex)`.
    pub(crate) fn request(self, key: DjmControlKey, option: u8) -> Option<(u16, u16)> {
        match key {
            DjmControlKey::CaptureLevel => self
                .levels()
                .get(usize::from(option))
                .map(|_| (u16::from(option) << 8, 0x8003)),
            DjmControlKey::CaptureSource(pair) => self
                .capture_sources(pair)?
                .get(usize::from(option))
                .map(|&value| ((u16::from(pair) << 8) | u16::from(value), 0x8002)),
            DjmControlKey::PlaybackDestination(pair)
                if self.has_playback() && (1..=3).contains(&pair) =>
            {
                PLAYBACK
                    .get(usize::from(option))
                    .map(|&value| ((u16::from(pair) << 8) | u16::from(value), 0x8016))
            }
            _ => None,
        }
    }

    fn capture_label(self, value: u8) -> &'static str {
        match (self, value) {
            (Self::Djm850, 0x00) => "Control Tone CD/LINE",
            (Self::Djm850, 0x01) => "Control Tone LINE",
            (_, 0x00) => "Control Tone LINE",
            (_, 0x01) => "Control Tone CD/LINE",
            (_, 0x02) => "Control Tone DIGITAL",
            (_, 0x03) => "Control Tone PHONO",
            (_, 0x06) => "Post Fader",
            (_, 0x07) => "Cross Fader A",
            (_, 0x08) => "Cross Fader B",
            (_, 0x09) => "Mic",
            (_, 0x0a) => "Rec Out",
            (_, 0x0d) => "Aux",
            (_, 0x0f) => "None",
            (_, 0x10) => "FX SEND",
            (_, 0x11) => "Post Fader Ch1",
            (_, 0x12) => "Post Fader Ch2",
            (_, 0x13) => "Post Fader Ch3",
            (_, 0x14) => "Post Fader Ch4",
            (_, 0x21) => "EXT1 SEND",
            (_, 0x22) => "EXT2 SEND",
            _ => unreachable!("capture option table has no ALSA label"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_identities_and_control_order() {
        let cases = [
            (DjmModel::Djm250Mk2, "djm-250mk2", 0x2b73, 0x0017, 3, 3),
            (DjmModel::Djm450, "djm-450", 0x2b73, 0x0013, 3, 3),
            (DjmModel::Djm750, "djm-750", 0x08e4, 0x017f, 4, 0),
            (DjmModel::Djm850, "djm-850", 0x08e4, 0x0163, 4, 0),
            (DjmModel::Djm900Nxs2, "djm-900nxs2", 0x2b73, 0x000a, 5, 0),
            (DjmModel::Djm750Mk2, "djm-750mk2", 0x2b73, 0x001b, 5, 3),
            (DjmModel::DjmV10, "djm-v10", 0x2b73, 0x0034, 6, 0),
        ];
        for (model, key, vendor, product, inputs, outputs) in cases {
            assert_eq!(model.key(), key);
            assert_eq!(DjmModel::from_key(key), Some(model));
            assert_eq!(DjmModel::from_usb_id(vendor, product), Some(model));
            assert_eq!(DjmModel::from_usb_id(0xffff, product), None);
            let controls = model.controls();
            let expected: Vec<_> = std::iter::once(DjmControlKey::CaptureLevel)
                .chain((1..=inputs).map(DjmControlKey::CaptureSource))
                .chain((1..=outputs).map(DjmControlKey::PlaybackDestination))
                .collect();
            assert_eq!(controls.iter().map(|c| c.key).collect::<Vec<_>>(), expected);
            for control in controls {
                for (ordinal, option) in control.options.iter().enumerate() {
                    assert_eq!(option.id, ordinal as u8);
                    assert!(model.request(control.key, option.id).is_some());
                    assert!(!option.label.is_empty());
                }
                assert_eq!(
                    model.request(control.key, control.options.len() as u8),
                    None
                );
            }
            assert_eq!(model.request(DjmControlKey::CaptureSource(0), 0), None);
            assert_eq!(
                model.request(DjmControlKey::CaptureSource(inputs + 1), 0),
                None
            );
            assert_eq!(
                model.request(DjmControlKey::PlaybackDestination(0), 0),
                None
            );
            assert_eq!(
                model.request(DjmControlKey::PlaybackDestination(outputs + 1), 0),
                None
            );
        }
        assert_eq!(DjmModel::from_key("djm-a9"), None);
        assert_eq!(DjmModel::from_usb_id(0x2b73, 0x003c), None);
    }

    #[test]
    fn alsa_capture_request_sequences() {
        use DjmModel::*;
        let cases: &[(DjmModel, &[&[u8]])] = &[
            (
                Djm250Mk2,
                &[
                    &[3, 0, 6, 7, 8, 9, 13, 10],
                    &[3, 0, 6, 7, 8, 9, 13, 10],
                    &[10, 17, 18, 7, 8, 9, 13],
                ],
            ),
            (
                Djm450,
                &[
                    &[3, 0, 6, 7, 8, 9, 13, 10],
                    &[3, 0, 6, 7, 8, 9, 13, 10],
                    &[10, 17, 18, 7, 8, 9, 13],
                ],
            ),
            (
                Djm750,
                &[
                    &[1, 3, 6, 7, 8, 9, 10, 15],
                    &[0, 1, 6, 7, 8, 9, 10, 15],
                    &[0, 1, 6, 7, 8, 9, 10, 15],
                    &[1, 3, 6, 7, 8, 9, 10, 15],
                ],
            ),
            (
                Djm850,
                &[
                    &[0, 3, 6, 7, 8, 9, 10, 15],
                    &[0, 1, 6, 7, 8, 9, 10, 15],
                    &[0, 1, 6, 7, 8, 9, 10, 15],
                    &[0, 3, 6, 7, 8, 9, 10, 15],
                ],
            ),
            (
                Djm900Nxs2,
                &[
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[7, 8, 9, 10, 17, 18, 19, 20],
                ],
            ),
            (
                Djm750Mk2,
                &[
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[0, 2, 3, 6, 7, 8, 9, 10],
                    &[7, 8, 9, 10, 17, 18, 19, 20],
                ],
            ),
            (
                DjmV10,
                &[
                    &[3, 0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                    &[0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                    &[3, 0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                    &[3, 0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                    &[0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                    &[3, 0, 2, 6, 16, 7, 8, 9, 10, 33, 34],
                ],
            ),
        ];
        for &(model, pairs) in cases {
            for (pair, values) in pairs.iter().enumerate() {
                let pair = (pair + 1) as u8;
                let control = model
                    .controls()
                    .into_iter()
                    .find(|c| c.key == DjmControlKey::CaptureSource(pair))
                    .unwrap();
                assert_eq!(control.options.len(), values.len());
                for (option, &value) in values.iter().enumerate() {
                    assert_eq!(
                        model.request(control.key, option as u8),
                        Some(((u16::from(pair) << 8) | u16::from(value), 0x8002))
                    );
                }
            }
        }
    }

    #[test]
    fn labels_levels_and_playback_match_alsa() {
        let controls = DjmModel::Djm850.controls();
        assert_eq!(controls[1].options[0].label, "Control Tone CD/LINE");
        assert_eq!(controls[2].options[1].label, "Control Tone LINE");
        for model in [
            DjmModel::Djm250Mk2,
            DjmModel::Djm450,
            DjmModel::Djm750,
            DjmModel::Djm850,
            DjmModel::Djm900Nxs2,
            DjmModel::Djm750Mk2,
            DjmModel::DjmV10,
        ] {
            let controls = model.controls();
            let expected = if model == DjmModel::DjmV10 {
                &["+15dB", "+12dB", "+9dB", "+6dB", "+3dB", "0dB"][..]
            } else {
                &["-19dB", "-15dB", "-10dB", "-5dB"][..]
            };
            assert_eq!(
                controls[0]
                    .options
                    .iter()
                    .map(|o| o.label)
                    .collect::<Vec<_>>(),
                expected
            );
            for option in &controls[0].options {
                assert_eq!(
                    model.request(DjmControlKey::CaptureLevel, option.id),
                    Some((u16::from(option.id) << 8, 0x8003))
                );
            }
            for control in controls
                .iter()
                .filter(|c| matches!(c.key, DjmControlKey::PlaybackDestination(_)))
            {
                let DjmControlKey::PlaybackDestination(pair) = control.key else {
                    unreachable!()
                };
                assert_eq!(
                    control.options.iter().map(|o| o.label).collect::<Vec<_>>(),
                    ["Ch1", "Ch2", "Aux"]
                );
                for (id, low) in [0u16, 1, 4].into_iter().enumerate() {
                    assert_eq!(
                        model.request(control.key, id as u8),
                        Some(((u16::from(pair) << 8) | low, 0x8016))
                    );
                }
            }
        }
    }
}
