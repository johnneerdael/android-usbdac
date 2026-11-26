//! The conversion path allocates nothing once a converter is built: every buffer it uses is made
//! in `Converter::new`. A counting allocator, armed only on the test's own thread, proves it for
//! each kind of plan.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use usbdac_core::convert::{ConversionPlan, ConvertOptions, Converter, PcmFormat, SampleFormat};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            let _ = COUNT.try_with(|count| count.set(count.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc_zeroed(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations_during(body: impl FnOnce()) -> u64 {
    COUNT.with(|count| count.set(0));
    ARMED.with(|armed| armed.set(true));
    body();
    ARMED.with(|armed| armed.set(false));
    COUNT.with(Cell::get)
}

fn plan(source: PcmFormat, target: PcmFormat) -> ConversionPlan {
    ConversionPlan::pcm(source, target, &ConvertOptions::default()).unwrap()
}

#[test]
fn processing_finishing_and_resetting_allocate_nothing() {
    let cases = [
        // integer engine: widening, narrowing with dither, mono to stereo
        plan(
            PcmFormat::new(44_100, 2, SampleFormat::I16),
            PcmFormat::new(44_100, 2, SampleFormat::I24),
        ),
        plan(
            PcmFormat::new(44_100, 2, SampleFormat::I32),
            PcmFormat::new(44_100, 2, SampleFormat::I16),
        ),
        plan(
            PcmFormat::new(44_100, 1, SampleFormat::I16),
            PcmFormat::new(44_100, 2, SampleFormat::I16),
        ),
        // float engine without resampling
        plan(
            PcmFormat::new(44_100, 2, SampleFormat::Float32),
            PcmFormat::new(44_100, 2, SampleFormat::I24),
        ),
        // resampling: upsampling, downsampling, the extremes, mono to stereo
        plan(
            PcmFormat::new(44_100, 2, SampleFormat::I16),
            PcmFormat::new(48_000, 2, SampleFormat::I16),
        ),
        plan(
            PcmFormat::new(96_000, 2, SampleFormat::I24),
            PcmFormat::new(44_100, 2, SampleFormat::I24),
        ),
        plan(
            PcmFormat::new(44_100, 1, SampleFormat::I16),
            PcmFormat::new(768_000, 2, SampleFormat::I32),
        ),
        plan(
            PcmFormat::new(768_000, 2, SampleFormat::I32),
            PcmFormat::new(44_100, 2, SampleFormat::I16),
        ),
    ];
    for plan in cases {
        let mut converter = Converter::new(&plan).unwrap();
        let source_frame = converter.source_frame_bytes();
        let input = vec![0x55u8; source_frame * 700];
        let mut output = vec![
            0u8;
            converter
                .min_output_bytes()
                .max(converter.target_frame_bytes() * 40_000)
        ];
        // Warm up once, then count.
        converter.process(&input, &mut output).unwrap();
        let allocations = allocations_during(|| {
            for _ in 0..50 {
                converter.process(&input, &mut output).unwrap();
            }
            while converter.finish(&mut output).unwrap() > 0 {}
            converter.reset();
            converter.process(&input, &mut output).unwrap();
        });
        assert_eq!(allocations, 0, "{}", plan.description());
    }
}

#[test]
fn the_counter_counts() {
    let allocations = allocations_during(|| {
        let buffer = vec![1u8; 100];
        std::hint::black_box(buffer);
    });
    assert!(
        allocations > 0,
        "the allocator hook does not see allocations, so the test above proves nothing"
    );
}
