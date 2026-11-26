use crate::ring_target::{live_processing, needs_refill, RingTargets};

// 48 kHz stereo: 96 samples per millisecond.
const SAMPLES_PER_MS: usize = 96;
const DEEP: usize = 250 * SAMPLES_PER_MS;
const LIVE: usize = 100 * SAMPLES_PER_MS;
const CHUNK: usize = 10 * SAMPLES_PER_MS;

// ---- which depth applies

#[test]
fn bit_perfect_passthrough_keeps_the_deep_ring() {
    let targets = RingTargets::new(DEEP, LIVE);
    let live = live_processing(false, 1.0);
    assert!(!live);
    assert_eq!(targets.select(live), DEEP);
}

#[test]
fn a_software_gain_keeps_the_shallow_ring() {
    let targets = RingTargets::new(DEEP, LIVE);
    assert_eq!(targets.select(live_processing(false, 0.5)), LIVE);
    assert_eq!(targets.select(live_processing(false, 1.5)), LIVE);
}

#[test]
fn only_a_gain_of_exactly_one_is_no_gain() {
    // The output thread applies the gain unless it is exactly 1.0.
    assert!(!live_processing(false, 1.0));
    assert!(live_processing(false, 1.0 - f32::EPSILON));
    assert!(live_processing(false, 1.0 + f32::EPSILON));
    assert!(live_processing(false, 0.0));
    // An unusable gain is not unity either.
    assert!(live_processing(false, f32::NAN));
}

#[test]
fn a_raw_bitstream_is_never_live_processing() {
    // The software gain is refused for DoP and native DSD; a bitstream keeps
    // the deep ring whatever the gain value says.
    assert!(!live_processing(true, 1.0));
    assert!(!live_processing(true, 0.5));
    assert!(!live_processing(true, 0.25));
}

#[test]
fn the_live_target_is_never_deeper_than_the_deep_one() {
    let targets = RingTargets::new(DEEP, 2 * DEEP);
    assert_eq!(targets.select(true), DEEP);
    assert_eq!(targets.select(false), DEEP);
    // A stream whose whole target is below the live depth keeps that target.
    let shallow = RingTargets::new(50 * SAMPLES_PER_MS, LIVE);
    assert_eq!(shallow.select(true), shallow.select(false));
}

#[test]
fn a_chunk_is_written_only_below_the_target() {
    assert!(needs_refill(0, DEEP));
    assert!(needs_refill(DEEP - 1, DEEP));
    assert!(!needs_refill(DEEP, DEEP));
    assert!(!needs_refill(DEEP + CHUNK, DEEP));
}

// ---- the ring across a change of mode

/// The host against the USB output, in 10 ms steps. The host is much faster than
/// real time (it renders a 10 ms chunk in far less than 10 ms) and writes while
/// the ring is below the target it has just been given; the output takes one
/// chunk per step. Nothing here ever clears the ring.
struct Ring {
    targets: RingTargets,
    fill: usize,
    history: Vec<usize>,
}

impl Ring {
    const WRITES_PER_STEP: usize = 8;

    fn new(fill: usize) -> Self {
        Self {
            targets: RingTargets::new(DEEP, LIVE),
            fill,
            history: vec![fill],
        }
    }

    /// One 10 ms step in the given mode.
    fn step(&mut self, live: bool) {
        for _ in 0..Self::WRITES_PER_STEP {
            if !needs_refill(self.fill, self.targets.select(live)) {
                break;
            }
            self.fill += CHUNK;
        }
        self.fill = self.fill.saturating_sub(CHUNK); // The output drains one chunk.
        self.history.push(self.fill);
    }

    fn run(&mut self, live: bool, steps: usize) {
        for _ in 0..steps {
            self.step(live);
        }
    }
}

#[test]
fn a_steady_ring_stays_at_the_target_of_its_mode() {
    let mut ring = Ring::new(0);
    ring.run(false, 100);
    assert!(ring.fill >= DEEP - CHUNK && ring.fill <= DEEP + CHUNK, "{}", ring.fill);

    let mut ring = Ring::new(0);
    ring.run(true, 100);
    assert!(ring.fill >= LIVE - CHUNK && ring.fill <= LIVE + CHUNK, "{}", ring.fill);
}

#[test]
fn going_live_stops_refilling_until_the_output_has_drained_to_the_shallow_target() {
    let mut ring = Ring::new(0);
    ring.run(false, 100);
    let at_switch = ring.history.len();

    ring.run(true, 40);

    // From the switch the fill only ever falls, one chunk at a time as the
    // output drains, until it reaches the shallow target. No flush: it never
    // jumps down, and nothing is written meanwhile.
    let after = &ring.history[at_switch - 1..];
    let reached = after
        .iter()
        .position(|&fill| fill <= LIVE)
        .expect("the ring drains to the shallow target");
    for pair in after[..=reached].windows(2) {
        assert_eq!(pair[0] - pair[1], CHUNK, "{pair:?}: one chunk drained, none written");
    }
    // It takes about the difference between the depths: no sooner.
    let drained_chunks = (DEEP - LIVE) / CHUNK;
    assert!(reached >= drained_chunks - 1 && reached <= drained_chunks + 2, "{reached}");
    // Then it holds at the shallow target.
    for &fill in &after[reached..] {
        assert!(fill >= LIVE - CHUNK && fill <= LIVE + CHUNK, "{fill}");
    }
}

#[test]
fn leaving_live_processing_fills_the_ring_on_to_the_deep_target() {
    let mut ring = Ring::new(0);
    ring.run(true, 100);
    let at_switch = ring.history.len();

    ring.run(false, 40);

    let after = &ring.history[at_switch - 1..];
    // It never falls on the way up: the host outruns the output.
    assert!(after.windows(2).all(|pair| pair[1] >= pair[0]), "{after:?}");
    // And it gets there within a few steps, not at the output's pace.
    let reached = after.iter().position(|&fill| fill >= DEEP - CHUNK).unwrap();
    assert!(reached <= 4, "took {reached} steps");
    assert!(after[reached..].iter().all(|&fill| fill >= DEEP - CHUNK && fill <= DEEP + CHUNK));
}

#[test]
fn the_mode_may_change_back_and_forth_without_the_ring_ever_jumping() {
    let mut ring = Ring::new(0);
    ring.run(false, 60);
    for live in [true, false, false, true, true, false, true, false] {
        ring.run(live, 7);
    }
    // One chunk drained, or up to the host's burst written, per step.
    for pair in ring.history.windows(2).skip(1) {
        let (before, after) = (pair[0] as isize, pair[1] as isize);
        let step = after - before;
        let chunk = CHUNK as isize;
        assert!(
            step >= -chunk && step <= (Ring::WRITES_PER_STEP as isize - 1) * chunk,
            "{pair:?}: a step of {step}"
        );
    }
}
