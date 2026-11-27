//! Rate-grid projection for standard UAC2 peers with a proven shared wire clock.
use super::{ImplicitFeedbackSource, SamplingFrequencySubrange};

pub(super) fn implicit_source_preference(
    source: &ImplicitFeedbackSource,
    out_alt: u8,
) -> (bool, usize, u8) {
    (
        source.alt_setting != out_alt,
        source.max_packet_bytes,
        source.alt_setting,
    )
}

fn capacity(source: &ImplicitFeedbackSource) -> Option<u32> {
    if source.bytes_per_frame == 0 || source.service_interval_us == 0 {
        return None;
    }
    // Use a wide intermediate even on hosts whose usize exceeds u32.
    let rate = (source.max_packet_bytes / source.bytes_per_frame) as u128 * 1_000_000
        / u128::from(source.service_interval_us);
    (rate > 0).then_some(rate.min(u128::from(u32::MAX)) as u32)
}

fn clip(
    range: &SamplingFrequencySubrange,
    low: u64,
    high: u32,
) -> Option<SamplingFrequencySubrange> {
    if range.min == 0 || range.max < range.min || low > u64::from(high) {
        return None;
    }
    if range.res == 0 {
        return (range.min == range.max && u64::from(range.min) >= low && range.min <= high)
            .then_some(*range);
    }
    let origin = u64::from(range.min);
    let step = u64::from(range.res);
    let lower = low.max(origin);
    let upper = u64::from(high.min(range.max));
    let first = origin + (lower - origin).div_ceil(step) * step;
    if first > upper {
        return None;
    }
    let last = origin + (upper - origin) / step * step;
    Some(SamplingFrequencySubrange {
        min: first as u32,
        max: last as u32,
        res: range.res,
    })
}

/// Match the pointwise implicit-source resolver without enumerating the clock grid.
/// `ranges` must already be clipped to the OUT packet capacity. Standard UAC2
/// peers share this proven grid; only their IN capacity ceilings differ.
pub(super) struct ImplicitRatePlan {
    pub(super) routes: Vec<(ImplicitFeedbackSource, Vec<SamplingFrequencySubrange>)>,
    pub(super) without_source: Vec<SamplingFrequencySubrange>,
}

pub(super) fn implicit_rate_routes(
    ranges: &[SamplingFrequencySubrange],
    matching: &[(ImplicitFeedbackSource, u8)],
    out_alt: u8,
) -> ImplicitRatePlan {
    let mut options: Vec<_> = matching
        .iter()
        .filter_map(|(source, rank)| capacity(source).map(|ceiling| (*source, *rank, ceiling)))
        .collect();
    // Stable ordering also retains the current resolver's input-order tie break.
    options.sort_by_key(|(source, rank, _)| (*rank, implicit_source_preference(source, out_alt)));
    let mut routes: Vec<(ImplicitFeedbackSource, Vec<SamplingFrequencySubrange>)> = Vec::new();
    let mut covered = Vec::new();
    for (index, (source, rank, ceiling)) in options.iter().enumerate() {
        let floor = options.iter().enumerate().filter(|(other_index, (other, other_rank, _))| {
            // Any stronger rank masks weaker ranks, including an ambiguous one.
            other_rank < rank || (other_rank == rank && (
                other.interface_number != source.interface_number || other.address != source.address
                // Within one physical pipe, the first surviving preferred alt wins.
                || *other_index < index))
        }).map(|(_, (_, _, other_ceiling))| *other_ceiling).max().unwrap_or(0);
        if floor < *ceiling {
            covered.push((u64::from(floor) + 1, u64::from(*ceiling)));
        }
        let clipped: Vec<_> = ranges
            .iter()
            .filter_map(|range| clip(range, u64::from(floor) + 1, *ceiling))
            .collect();
        if clipped.is_empty() {
            continue;
        }
        if let Some((_, existing)) = routes.iter_mut().find(|(other, _)| other == source) {
            existing.extend(clipped);
        } else {
            routes.push((*source, clipped));
        }
    }
    // Complement the union before projecting onto grids: an off-grid interval
    // still masks nominal selection, but cannot create a false grid point.
    covered.sort_unstable();
    let mut without_source = Vec::new();
    let mut next = 1u64;
    for (low, high) in covered {
        if next < low {
            without_source.extend(
                ranges
                    .iter()
                    .filter_map(|range| clip(range, next, (low - 1) as u32)),
            );
        }
        next = next.max(high + 1);
    }
    if next <= u64::from(u32::MAX) {
        without_source.extend(
            ranges
                .iter()
                .filter_map(|range| clip(range, next, u32::MAX)),
        );
    }
    ImplicitRatePlan {
        routes,
        without_source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(pipe: u8, alt: u8, frames: usize) -> ImplicitFeedbackSource {
        ImplicitFeedbackSource {
            interface_number: pipe,
            address: 0x80 | pipe,
            alt_setting: alt,
            bytes_per_frame: 4,
            service_interval_us: 1_000_000,
            max_packet_bytes: frames * 4,
        }
    }

    fn contains(range: &SamplingFrequencySubrange, rate: u32) -> bool {
        if range.res == 0 {
            range.min == rate && range.max == rate
        } else {
            rate >= range.min && rate <= range.max && (rate - range.min) % range.res == 0
        }
    }

    fn exact(
        matching: &[(ImplicitFeedbackSource, u8)],
        out_alt: u8,
        rate: u32,
    ) -> Option<ImplicitFeedbackSource> {
        let fitting = matching
            .iter()
            .copied()
            .filter(|(source, _)| {
                source.bytes_per_frame > 0
                    && source.service_interval_us > 0
                    && super::super::scan::required_max_packet_bytes(
                        rate,
                        source.service_interval_us,
                        source.bytes_per_frame,
                    ) <= source.max_packet_bytes
            })
            .collect();
        super::super::scan::choose_implicit_source(fitting, out_alt)
    }

    #[test]
    fn routes_match_pointwise_rank_resolution() {
        let grids = [
            SamplingFrequencySubrange {
                min: 3,
                max: 63,
                res: 4,
            },
            SamplingFrequencySubrange {
                min: 70,
                max: 100,
                res: 7,
            },
            SamplingFrequencySubrange {
                min: 69,
                max: 69,
                res: 0,
            },
        ];
        let cases = [
            vec![],
            vec![(source(1, 1, 100), 0), (source(2, 2, 100), 0)],
            vec![(source(1, 1, 20), 0), (source(1, 2, 80), 1)],
            vec![(source(1, 1, 20), 0), (source(1, 2, 80), 0)],
            vec![
                (source(1, 1, 60), 0),
                (source(2, 2, 40), 0),
                (source(3, 3, 100), 1),
            ],
            vec![
                (source(1, 1, 20), 0),
                (source(1, 2, 80), 0),
                (source(2, 3, 40), 0),
            ],
        ];
        for matching in &cases {
            for out_alt in [1, 2, 9] {
                let plan = implicit_rate_routes(&grids, matching, out_alt);
                for rate in 1..=110 {
                    let actual: Vec<_> = plan
                        .routes
                        .iter()
                        .filter(|(_, ranges)| ranges.iter().any(|range| contains(range, rate)))
                        .map(|(source, _)| *source)
                        .collect();
                    let expected = grids
                        .iter()
                        .any(|range| contains(range, rate))
                        .then(|| exact(matching, out_alt, rate))
                        .flatten();
                    assert_eq!(
                        actual,
                        expected.into_iter().collect::<Vec<_>>(),
                        "rate={rate}, alt={out_alt}, matching={matching:?}"
                    );
                    let gaps = plan
                        .without_source
                        .iter()
                        .filter(|range| contains(range, rate))
                        .count();
                    let on_grid = grids.iter().any(|range| contains(range, rate));
                    assert_eq!(
                        gaps,
                        usize::from(on_grid && expected.is_none()),
                        "nominal rate={rate}, alt={out_alt}, matching={matching:?}"
                    );
                    assert_eq!(actual.len() + gaps, usize::from(on_grid));
                }
            }
        }
    }

    #[test]
    fn clips_grids_and_handles_capacity_and_boundary_overflow() {
        let grid = SamplingFrequencySubrange {
            min: u32::MAX - 10,
            max: u32::MAX,
            res: 3,
        };
        let clipped = clip(&grid, u64::from(u32::MAX - 8), u32::MAX - 2).unwrap();
        assert_eq!(
            (clipped.min, clipped.max, clipped.res),
            (u32::MAX - 7, u32::MAX - 4, 3)
        );
        assert!(clip(&grid, u64::from(u32::MAX) + 1, u32::MAX).is_none());
        let mut huge = source(1, 1, 1);
        huge.max_packet_bytes = usize::MAX;
        huge.bytes_per_frame = 1;
        huge.service_interval_us = 1;
        assert_eq!(capacity(&huge), Some(u32::MAX));
        let grids = [
            grid,
            SamplingFrequencySubrange {
                min: u32::MAX,
                max: u32::MAX,
                res: 0,
            },
        ];
        let matching = [(huge, 0), (source(2, 2, 100), 1)];
        let plan = implicit_rate_routes(&grids, &matching, 1);
        assert!(plan.without_source.is_empty());
        let routes = plan.routes;
        assert_eq!(routes.len(), 1);
        for rate in [
            u32::MAX - 10,
            u32::MAX - 7,
            u32::MAX - 4,
            u32::MAX - 1,
            u32::MAX,
        ] {
            assert_eq!(
                routes[0].1.iter().any(|range| contains(range, rate)),
                grids.iter().any(|range| contains(range, rate))
            );
        }
        huge.bytes_per_frame = 0;
        let plan = implicit_rate_routes(&grids, &[(huge, 0)], 1);
        assert!(plan.routes.is_empty());
        assert_eq!(plan.without_source.len(), grids.len());
        huge.bytes_per_frame = 1;
        huge.service_interval_us = 0;
        let plan = implicit_rate_routes(&grids, &[(huge, 0)], 1);
        assert!(plan.routes.is_empty());
        assert_eq!(plan.without_source.len(), grids.len());
    }
}
