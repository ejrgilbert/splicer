//! Relocation-resolver tests for [`super::super::blob`]. Many-segments
//! × many-relocs threading + coalesced-segment local-offset wiring.

use super::super::blob::{Reloc, RelocPlan, SymbolBases, SymbolId};

/// Many segments × many relocs — guards the seg_idx threading.
#[test]
fn resolve_writes_each_site_in_owning_segment() {
    const N: u32 = 64;
    const SLOTS_PER_SEG: u32 = 8;
    const SEG_BYTES: u32 = SLOTS_PER_SEG * 4;
    // Leave a 4-byte gap between segments so they don't coalesce
    // and seg_idx == placement order.
    const STRIDE: u32 = SEG_BYTES + 4;

    let mut symbols = SymbolBases::new();
    let mut plan = RelocPlan::new();
    let mut data_segments: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut targets: Vec<SymbolId> = Vec::new();

    for i in 0..N {
        let id = symbols.alloc();
        let base = i * STRIDE;
        symbols.set(id, base);
        data_segments.push((base, vec![0u8; SEG_BYTES as usize]));
        targets.push(id);
    }

    // Each segment patches all 8 slots, each pointing at a
    // different segment's base + a unique addend.
    for (seg_idx, (base, _)) in data_segments.iter().enumerate() {
        let relocs: Vec<Reloc> = (0..SLOTS_PER_SEG)
            .map(|s| Reloc {
                site: s * 4,
                target: targets[(seg_idx + s as usize) % N as usize],
                addend: (seg_idx as i32) * 100 + s as i32,
            })
            .collect();
        plan.record_segment(seg_idx, *base, relocs);
    }

    plan.resolve(&symbols, &mut data_segments);

    for (seg_idx, (_, bytes)) in data_segments.iter().enumerate() {
        for s in 0..SLOTS_PER_SEG as usize {
            let off = s * 4;
            let written = i32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
            let target_base = ((seg_idx + s) % N as usize) as i32 * STRIDE as i32;
            let addend = (seg_idx as i32) * 100 + s as i32;
            assert_eq!(written, target_base + addend);
        }
    }
}

/// Coalesced placements share an entry; each reloc still hits
/// the right local offset.
#[test]
fn resolve_handles_coalesced_segments() {
    let mut symbols = SymbolBases::new();
    let mut plan = RelocPlan::new();

    let id_a = symbols.alloc();
    let id_b = symbols.alloc();
    symbols.set(id_a, 0);
    symbols.set(id_b, 8);

    // Single coalesced entry holds both placements.
    let mut data_segments = vec![(0u32, vec![0u8; 16])];

    // Placement A: bytes 0..8, site at offset 4 → target B (=8).
    plan.record_segment(
        0,
        0,
        vec![Reloc {
            site: 4,
            target: id_b,
            addend: 0,
        }],
    );
    // Placement B: bytes 8..16, site at offset 0 → target A (=0) + 3.
    plan.record_segment(
        0,
        8,
        vec![Reloc {
            site: 0,
            target: id_a,
            addend: 3,
        }],
    );

    plan.resolve(&symbols, &mut data_segments);

    let bytes = &data_segments[0].1;
    assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 8);
    assert_eq!(i32::from_le_bytes(bytes[8..12].try_into().unwrap()), 3);
}
