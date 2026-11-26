//! First-generation Focusrite UAC2 routing, from pinned ALSA mixer_scarlett.c.
//! Values are signed wire values: routes use -1 for Off; gains use 1/256 dB.
//! Reading never initializes the mixer, clock, sample rate or preamps.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gen1Model {
    Scarlett6i6,
    Scarlett8i6,
    Scarlett18i6,
    Scarlett18i8,
    Scarlett18i20,
    Forte,
}

impl Gen1Model {
    pub fn recognize(vendor: u16, product: u16) -> Option<Self> {
        if vendor != 0x1235 {
            return None;
        }
        match product {
            0x8012 => Some(Self::Scarlett6i6),
            0x8002 => Some(Self::Scarlett8i6),
            0x8004 => Some(Self::Scarlett18i6),
            0x8014 => Some(Self::Scarlett18i8),
            0x800c => Some(Self::Scarlett18i20),
            0x8010 => Some(Self::Forte),
            _ => None,
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Self::Scarlett6i6 => "scarlett_6i6_gen1",
            Self::Scarlett8i6 => "scarlett_8i6_gen1",
            Self::Scarlett18i6 => "scarlett_18i6_gen1",
            Self::Scarlett18i8 => "scarlett_18i8_gen1",
            Self::Scarlett18i20 => "scarlett_18i20_gen1",
            Self::Forte => "forte",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Scarlett6i6 => "Scarlett 6i6 Gen 1",
            Self::Scarlett8i6 => "Scarlett 8i6 Gen 1",
            Self::Scarlett18i6 => "Scarlett 18i6 Gen 1",
            Self::Scarlett18i8 => "Scarlett 18i8 Gen 1",
            Self::Scarlett18i20 => "Scarlett 18i20 Gen 1",
            Self::Forte => "Forte",
        }
    }
    fn layout(self) -> Layout {
        let (inputs, mixes, captures, outputs, matrix_sources, output_sources, offsets) = match self
        {
            Self::Scarlett6i6 => (18, 8, 6, 6, 19, 27, [0, 12, 16, 18, 18]),
            Self::Scarlett8i6 => (18, 6, 8, 6, 19, 25, [0, 12, 16, 18, 18]),
            Self::Scarlett18i6 => (18, 6, 18, 6, 25, 31, [0, 6, 14, 16, 24]),
            Self::Scarlett18i8 => (18, 8, 18, 8, 27, 35, [0, 8, 16, 18, 26]),
            Self::Scarlett18i20 => (18, 8, 18, 20, 39, 47, [0, 20, 28, 30, 38]),
            // Forte's initializer creates no capture or output route controls.
            Self::Forte => (6, 4, 0, 0, 7, 0, [0, 4, 6, 6, 6]),
        };
        Layout {
            inputs,
            mixes,
            captures,
            outputs,
            matrix_sources,
            output_sources,
            offsets,
        }
    }
}

struct Layout {
    inputs: u8,
    mixes: u8,
    captures: u8,
    outputs: u8,
    matrix_sources: u8,
    output_sources: u8,
    offsets: [u8; 5],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gen1Key {
    MatrixInput(u8),
    Output(u8),
    Capture(u8),
    Gain { input: u8, mix: u8 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gen1Control {
    pub key: Gen1Key,
    pub value: i16,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gen1Readback {
    pub model: Gen1Model,
    pub controls: Vec<Gen1Control>,
    pub source_offsets: [u8; 5],
    pub matrix_sources: u8,
    pub output_sources: u8,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gen1Edit {
    pub key: Gen1Key,
    pub value: i16,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gen1Report {
    pub original: Gen1Readback,
    pub verified: Gen1Readback,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Gen1Failure<E> {
    Io { key: Gen1Key, error: E },
    Invalid { key: Gen1Key },
    Drift { key: Option<Gen1Key> },
    Verification { key: Gen1Key },
}
#[derive(Debug, PartialEq, Eq)]
pub struct Gen1WriteError<E> {
    pub failure: Gen1Failure<E>,
    pub rollback: Vec<(Gen1Key, Gen1Failure<E>)>,
}
pub trait Gen1Io {
    type Error;
    fn read(&mut self, entity: u8, value: u16) -> Result<i16, Self::Error>;
    fn write(&mut self, entity: u8, value: u16, data: i16) -> Result<(), Self::Error>;
}

fn keys(model: Gen1Model) -> Vec<Gen1Key> {
    let l = model.layout();
    let mut keys = Vec::new();
    for input in 1..=l.inputs {
        keys.push(Gen1Key::MatrixInput(input));
        for mix in 1..=l.mixes {
            keys.push(Gen1Key::Gain { input, mix });
        }
    }
    keys.extend((1..=l.outputs).map(Gen1Key::Output));
    keys.extend((1..=l.captures).map(Gen1Key::Capture));
    keys
}

fn address(model: Gen1Model, key: Gen1Key) -> Option<(u8, u16)> {
    let l = model.layout();
    match key {
        Gen1Key::MatrixInput(n) if n > 0 && n <= l.inputs => {
            Some((0x32, 0x0600 | u16::from(n - 1)))
        }
        Gen1Key::Output(n) if n > 0 && n <= l.outputs => Some((0x33, u16::from(n - 1))),
        Gen1Key::Capture(n) if n > 0 && n <= l.captures => Some((0x34, u16::from(n - 1))),
        Gen1Key::Gain { input, mix }
            if input > 0 && input <= l.inputs && mix > 0 && mix <= l.mixes =>
        {
            let (selector, shift) = if model == Gen1Model::Forte {
                (0x0100, 2)
            } else {
                (0, 3)
            };
            Some((
                0x3c,
                selector | (u16::from(input - 1) << shift) | u16::from(mix - 1),
            ))
        }
        _ => None,
    }
}

fn valid_value(model: Gen1Model, key: Gen1Key, value: i16, edit: bool) -> bool {
    if address(model, key).is_none() {
        return false;
    }
    let l = model.layout();
    match key {
        Gen1Key::Gain { .. } => !edit || (value <= 6 * 256 && value % 256 == 0),
        Gen1Key::MatrixInput(_) => value >= -1 && value < i16::from(l.matrix_sources) - 1,
        _ => value >= -1 && value < i16::from(l.output_sources) - 1,
    }
}

fn read_one<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
    key: Gen1Key,
) -> Result<i16, Gen1Failure<I::Error>> {
    let (entity, value) = address(model, key).ok_or(Gen1Failure::Invalid { key })?;
    io.read(entity, value)
        .map_err(|error| Gen1Failure::Io { key, error })
}
fn write_one<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
    key: Gen1Key,
    data: i16,
) -> Result<(), Gen1Failure<I::Error>> {
    let (entity, value) = address(model, key).ok_or(Gen1Failure::Invalid { key })?;
    io.write(entity, value, data)
        .map_err(|error| Gen1Failure::Io { key, error })
}

pub fn read_gen1<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
) -> Result<Gen1Readback, Gen1Failure<I::Error>> {
    let l = model.layout();
    let mut controls = Vec::new();
    for key in keys(model) {
        let value = read_one(io, model, key)?;
        if !valid_value(model, key, value, false) {
            return Err(Gen1Failure::Invalid { key });
        }
        controls.push(Gen1Control { key, value });
    }
    Ok(Gen1Readback {
        model,
        controls,
        source_offsets: l.offsets,
        matrix_sources: l.matrix_sources,
        output_sources: l.output_sources,
    })
}

fn complete(r: &Gen1Readback) -> bool {
    let l = r.model.layout();
    r.source_offsets == l.offsets
        && r.matrix_sources == l.matrix_sources
        && r.output_sources == l.output_sources
        && r.controls.iter().map(|c| c.key).eq(keys(r.model))
        && r.controls
            .iter()
            .all(|c| valid_value(r.model, c.key, c.value, false))
}
pub(crate) fn same_gen1_layout(a: &Gen1Readback, b: &Gen1Readback) -> bool {
    a.model == b.model && complete(a) && complete(b)
}

// Only undo values still held by this transaction. Failed writes are included:
// a transport error may have occurred after the device accepted the transfer.
fn rollback<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
    changed: &[(Gen1Key, i16, i16)],
) -> Vec<(Gen1Key, Gen1Failure<I::Error>)> {
    let mut errors = Vec::new();
    for &(key, original, held) in changed.iter().rev() {
        let result = (|| {
            let live = read_one(io, model, key)?;
            if live == original {
                return Ok(());
            }
            if live != held {
                return Err(Gen1Failure::Drift { key: Some(key) });
            }
            let write = write_one(io, model, key, original);
            match read_one(io, model, key) {
                Ok(value) if value == original => Ok(()),
                read => {
                    write?;
                    read?;
                    Err(Gen1Failure::Verification { key })
                }
            }
        })();
        if let Err(error) = result {
            errors.push((key, error));
        }
    }
    errors
}

pub fn apply_gen1<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
    expected: &Gen1Readback,
    edits: &[Gen1Edit],
) -> Result<Gen1Report, Gen1WriteError<I::Error>> {
    let fail = |failure| Gen1WriteError {
        failure,
        rollback: Vec::new(),
    };
    if expected.model != model || !complete(expected) {
        return Err(fail(Gen1Failure::Drift { key: None }));
    }
    for (i, edit) in edits.iter().enumerate() {
        if !valid_value(model, edit.key, edit.value, true)
            || edits[..i].iter().any(|e| e.key == edit.key)
        {
            return Err(fail(Gen1Failure::Invalid { key: edit.key }));
        }
    }
    let original = read_gen1(io, model).map_err(fail)?;
    if &original != expected {
        return Err(fail(Gen1Failure::Drift { key: None }));
    }
    let mut desired = original.clone();
    let mut changed = Vec::new();
    let result = (|| {
        for edit in edits {
            let control = desired
                .controls
                .iter_mut()
                .find(|c| c.key == edit.key)
                .expect("validated key");
            if read_one(io, model, edit.key)? != control.value {
                return Err(Gen1Failure::Drift {
                    key: Some(edit.key),
                });
            }
            if control.value == edit.value {
                continue;
            }
            changed.push((edit.key, control.value, edit.value));
            write_one(io, model, edit.key, edit.value)?;
            if read_one(io, model, edit.key)? != edit.value {
                return Err(Gen1Failure::Verification { key: edit.key });
            }
            control.value = edit.value;
        }
        let verified = read_gen1(io, model)?;
        if verified != desired {
            return Err(Gen1Failure::Drift { key: None });
        }
        Ok(verified)
    })();
    match result {
        Ok(verified) => Ok(Gen1Report { original, verified }),
        Err(failure) => Err(Gen1WriteError {
            failure,
            rollback: rollback(io, model, &changed),
        }),
    }
}

pub(crate) fn restore_gen1<I: Gen1Io>(
    io: &mut I,
    model: Gen1Model,
    last: &Gen1Readback,
    original: &Gen1Readback,
) -> Result<(), Gen1WriteError<I::Error>> {
    if model != last.model || !same_gen1_layout(last, original) {
        return Err(Gen1WriteError {
            failure: Gen1Failure::Drift { key: None },
            rollback: Vec::new(),
        });
    }
    let changes: Vec<_> = original
        .controls
        .iter()
        .zip(&last.controls)
        .filter(|(a, b)| a.value != b.value)
        .map(|(a, b)| (a.key, a.value, b.value))
        .collect();
    let mut errors = rollback(io, model, &changes).into_iter();
    if let Some((_, failure)) = errors.next() {
        Err(Gen1WriteError {
            failure,
            rollback: errors.collect(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Io {
        values: BTreeMap<(u8, u16), i16>,
        writes: Vec<(u8, u16, i16)>,
        fail_write: Option<usize>,
        external_on_failure: Option<((u8, u16), i16)>,
    }
    impl Io {
        fn new(model: Gen1Model) -> Self {
            Self {
                values: keys(model)
                    .into_iter()
                    .map(|key| {
                        (
                            address(model, key).unwrap(),
                            if matches!(key, Gen1Key::Gain { .. }) {
                                -12345
                            } else {
                                -1
                            },
                        )
                    })
                    .collect(),
                writes: Vec::new(),
                fail_write: None,
                external_on_failure: None,
            }
        }
    }
    impl Gen1Io for Io {
        type Error = &'static str;
        fn read(&mut self, entity: u8, value: u16) -> Result<i16, Self::Error> {
            self.values
                .get(&(entity, value))
                .copied()
                .ok_or("unexpected address")
        }
        fn write(&mut self, entity: u8, value: u16, data: i16) -> Result<(), Self::Error> {
            self.writes.push((entity, value, data));
            self.values.insert((entity, value), data);
            if self.fail_write == Some(self.writes.len()) {
                if let Some((address, data)) = self.external_on_failure {
                    self.values.insert(address, data);
                }
                return Err("uncertain transfer");
            }
            Ok(())
        }
    }

    #[test]
    fn exact_model_and_wire_geometry() {
        for (product, model, inputs, mixes, captures, outputs, matrix_sources, output_sources) in [
            (0x8012, Gen1Model::Scarlett6i6, 18, 8, 6, 6, 19, 27),
            (0x8002, Gen1Model::Scarlett8i6, 18, 6, 8, 6, 19, 25),
            (0x8004, Gen1Model::Scarlett18i6, 18, 6, 18, 6, 25, 31),
            (0x8014, Gen1Model::Scarlett18i8, 18, 8, 18, 8, 27, 35),
            (0x800c, Gen1Model::Scarlett18i20, 18, 8, 18, 20, 39, 47),
            (0x8010, Gen1Model::Forte, 6, 4, 0, 0, 7, 0),
        ] {
            assert_eq!(Gen1Model::recognize(0x1235, product), Some(model));
            assert_eq!(Gen1Model::recognize(0xffff, product), None);
            let mut io = Io::new(model);
            let r = read_gen1(&mut io, model).unwrap();
            assert_eq!(r.controls.len(), inputs * (mixes + 1) + captures + outputs);
            assert_eq!(
                (r.matrix_sources, r.output_sources),
                (matrix_sources, output_sources)
            );
            assert!(io.writes.is_empty());
        }
        assert_eq!(Gen1Model::recognize(0x1235, 0x8215), None);
        let s = Gen1Model::Scarlett18i20;
        assert_eq!(address(s, Gen1Key::MatrixInput(18)), Some((0x32, 0x0611)));
        assert_eq!(
            address(s, Gen1Key::Gain { input: 18, mix: 8 }),
            Some((0x3c, 0x008f))
        );
        assert_eq!(address(s, Gen1Key::Output(20)), Some((0x33, 19)));
        assert_eq!(address(s, Gen1Key::Capture(18)), Some((0x34, 17)));
        let f = Gen1Model::Forte;
        assert_eq!(
            address(f, Gen1Key::Gain { input: 6, mix: 4 }),
            Some((0x3c, 0x0117))
        );
        assert_eq!(address(f, Gen1Key::MatrixInput(6)), Some((0x32, 0x0605)));
        assert_eq!(address(f, Gen1Key::Output(1)), None);
        assert_eq!(address(f, Gen1Key::Capture(1)), None);
        assert_eq!(address(s, Gen1Key::MatrixInput(0)), None);
    }

    #[test]
    fn signed_off_and_one_db_gain_writes_preserve_raw_original() {
        let model = Gen1Model::Forte;
        let mut io = Io::new(model);
        let initial = read_gen1(&mut io, model).unwrap();
        let edits = [
            Gen1Edit {
                key: Gen1Key::MatrixInput(6),
                value: 5,
            },
            Gen1Edit {
                key: Gen1Key::Gain { input: 6, mix: 4 },
                value: 1536,
            },
        ];
        let report = apply_gen1(&mut io, model, &initial, &edits).unwrap();
        assert_eq!(io.writes, vec![(0x32, 0x0605, 5), (0x3c, 0x0117, 1536)]);
        restore_gen1(&mut io, model, &report.verified, &report.original).unwrap();
        assert_eq!(
            &io.writes[2..],
            &[(0x3c, 0x0117, -12345), (0x32, 0x0605, -1)]
        );
        for value in [-32768, -256, 0, 1536] {
            assert!(valid_value(model, edits[1].key, value, true));
        }
        for value in [-32767, 1, 1537, 1792] {
            assert!(!valid_value(model, edits[1].key, value, true));
        }
        assert!(!valid_value(model, edits[0].key, 6, true));
    }

    #[test]
    fn uncertain_write_rolls_back_in_reverse_and_preserves_external_change() {
        let model = Gen1Model::Scarlett6i6;
        let mut io = Io::new(model);
        let initial = read_gen1(&mut io, model).unwrap();
        let edits = [
            Gen1Edit {
                key: Gen1Key::Output(1),
                value: 0,
            },
            Gen1Edit {
                key: Gen1Key::Capture(1),
                value: 1,
            },
        ];
        io.fail_write = Some(2);
        let failure = apply_gen1(&mut io, model, &initial, &edits).unwrap_err();
        assert!(matches!(
            failure.failure,
            Gen1Failure::Io {
                key: Gen1Key::Capture(1),
                ..
            }
        ));
        assert!(failure.rollback.is_empty());
        assert_eq!(
            io.writes,
            vec![(0x33, 0, 0), (0x34, 0, 1), (0x34, 0, -1), (0x33, 0, -1)]
        );
        io.writes.clear();
        io.external_on_failure = Some(((0x33, 0), 4));
        let failure = apply_gen1(&mut io, model, &initial, &edits).unwrap_err();
        assert_eq!(failure.rollback.len(), 1);
        assert_eq!(failure.rollback[0].0, Gen1Key::Output(1));
        assert_eq!(io.values[&(0x33, 0)], 4);
        assert_eq!(io.values[&(0x34, 0)], -1);
    }

    #[test]
    fn full_snapshot_and_all_edit_validation_precede_writes() {
        let model = Gen1Model::Scarlett18i8;
        let mut io = Io::new(model);
        let initial = read_gen1(&mut io, model).unwrap();
        let edits = [Gen1Edit {
            key: Gen1Key::Output(1),
            value: 0,
        }];
        io.values.insert((0x3c, 0x008f), -10000);
        assert!(matches!(
            apply_gen1(&mut io, model, &initial, &edits)
                .unwrap_err()
                .failure,
            Gen1Failure::Drift { .. }
        ));
        assert!(io.writes.is_empty());
        let fresh = read_gen1(&mut io, model).unwrap();
        let invalid = [
            edits[0],
            Gen1Edit {
                key: Gen1Key::Gain { input: 1, mix: 1 },
                value: 1,
            },
        ];
        assert!(matches!(
            apply_gen1(&mut io, model, &fresh, &invalid)
                .unwrap_err()
                .failure,
            Gen1Failure::Invalid { .. }
        ));
        assert!(io.writes.is_empty());
        let mut incomplete = fresh.clone();
        incomplete.controls.pop();
        assert!(!same_gen1_layout(&fresh, &incomplete));
    }

    #[test]
    fn close_restores_independent_held_controls_despite_external_drift() {
        let model = Gen1Model::Forte;
        let mut io = Io::new(model);
        let original = read_gen1(&mut io, model).unwrap();
        let report = apply_gen1(
            &mut io,
            model,
            &original,
            &[
                Gen1Edit {
                    key: Gen1Key::MatrixInput(1),
                    value: 0,
                },
                Gen1Edit {
                    key: Gen1Key::MatrixInput(2),
                    value: 1,
                },
            ],
        )
        .unwrap();
        io.values.insert((0x32, 0x0601), 3);
        assert!(restore_gen1(&mut io, model, &report.verified, &original).is_err());
        assert_eq!(io.values[&(0x32, 0x0600)], -1);
        assert_eq!(io.values[&(0x32, 0x0601)], 3);
    }

    #[test]
    fn close_accepts_exact_readback_after_lost_restore_ack() {
        let model = Gen1Model::Forte;
        let mut io = Io::new(model);
        let original = read_gen1(&mut io, model).unwrap();
        let report = apply_gen1(
            &mut io,
            model,
            &original,
            &[Gen1Edit {
                key: Gen1Key::Gain { input: 1, mix: 1 },
                value: 0,
            }],
        )
        .unwrap();
        io.fail_write = Some(2);
        restore_gen1(&mut io, model, &report.verified, &original).unwrap();
        assert_eq!(io.values[&(0x3c, 0x0100)], -12345);
    }
}
