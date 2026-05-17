//! Canonical Morton (Z-order) encoding for voxel terrain chunk keys.
//!
//! Per §4.8b conventions:
//! - 21 bits per axis, zyx interleave (z bits at positions 2,5,8,…; x at 0,3,6,…).
//! - Signed chunk coordinates are biased to unsigned by `+ CHUNK_MORTON_BIAS`
//!   before interleaving so negative coords round-trip cleanly.
//! - Per-voxel addressing within a chunk is a flat `voxel_idx = x + y*N + z*N*N`,
//!   NOT its own Morton (voxels are processed linearly by the mesher).
//!
//! Locking the encoder/decoder in one place avoids the silent-corruption class
//! of bug where two callers disagree on bit ordering or bias.

/// Bias added to each signed chunk axis before interleaving (`1 << 20`).
/// Together with the 21-bit per-axis budget this gives the legal range
/// `-CHUNK_MORTON_BIAS ..= CHUNK_MORTON_BIAS - 1` per axis.
pub const CHUNK_MORTON_BIAS: i32 = 1 << 20;

/// Maximum (exclusive) biased magnitude per axis.
const AXIS_MASK: u64 = (1u64 << 21) - 1;

/// Spread the low 21 bits of `v` across every third bit, leaving 0s in between.
/// Bit `i` of the input lands at bit `3*i` of the output.
#[inline]
fn split_by_3(v: u64) -> u64 {
    let mut x = v & AXIS_MASK;
    x = (x | (x << 32)) & 0x001f_0000_0000_ffff;
    x = (x | (x << 16)) & 0x001f_0000_ff00_00ff;
    x = (x | (x << 8)) & 0x100f_00f0_0f00_f00f;
    x = (x | (x << 4)) & 0x10c3_0c30_c30c_30c3;
    x = (x | (x << 2)) & 0x1249_2492_4924_9249;
    x
}

/// Inverse of [`split_by_3`]: gather every third bit back into the low 21.
#[inline]
fn compact_by_3(v: u64) -> u64 {
    let mut x = v & 0x1249_2492_4924_9249;
    x = (x ^ (x >> 2)) & 0x10c3_0c30_c30c_30c3;
    x = (x ^ (x >> 4)) & 0x100f_00f0_0f00_f00f;
    x = (x ^ (x >> 8)) & 0x001f_0000_ff00_00ff;
    x = (x ^ (x >> 16)) & 0x001f_0000_0000_ffff;
    x = (x ^ (x >> 32)) & AXIS_MASK;
    x
}

/// Encode a signed chunk coordinate as a Morton key (zyx interleave).
///
/// Panics if any axis falls outside `-CHUNK_MORTON_BIAS .. CHUNK_MORTON_BIAS`.
#[inline]
pub fn encode_chunk_morton(cx: i32, cy: i32, cz: i32) -> u64 {
    assert!(
        cx >= -CHUNK_MORTON_BIAS && cx < CHUNK_MORTON_BIAS,
        "chunk_x {cx} out of Morton range",
    );
    assert!(
        cy >= -CHUNK_MORTON_BIAS && cy < CHUNK_MORTON_BIAS,
        "chunk_y {cy} out of Morton range",
    );
    assert!(
        cz >= -CHUNK_MORTON_BIAS && cz < CHUNK_MORTON_BIAS,
        "chunk_z {cz} out of Morton range",
    );
    let ux = (cx as i64 + CHUNK_MORTON_BIAS as i64) as u64;
    let uy = (cy as i64 + CHUNK_MORTON_BIAS as i64) as u64;
    let uz = (cz as i64 + CHUNK_MORTON_BIAS as i64) as u64;
    split_by_3(ux) | (split_by_3(uy) << 1) | (split_by_3(uz) << 2)
}

/// Decode a Morton key back to signed chunk coordinates.
#[inline]
pub fn decode_chunk_morton(m: u64) -> (i32, i32, i32) {
    let ux = compact_by_3(m);
    let uy = compact_by_3(m >> 1);
    let uz = compact_by_3(m >> 2);
    (
        (ux as i64 - CHUNK_MORTON_BIAS as i64) as i32,
        (uy as i64 - CHUNK_MORTON_BIAS as i64) as i32,
        (uz as i64 - CHUNK_MORTON_BIAS as i64) as i32,
    )
}

/// Flat per-voxel index inside a chunk of side `n`: `x + y*n + z*n*n`.
#[inline]
pub fn voxel_idx(x: u32, y: u32, z: u32, n: u32) -> u32 {
    debug_assert!(x < n && y < n && z < n, "voxel ({x},{y},{z}) out of chunk side {n}");
    x + y * n + z * n * n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let cases = [
            (0, 0, 0),
            (1, 0, 0),
            (0, 1, 0),
            (0, 0, 1),
            (-1, 0, 0),
            (0, -1, 0),
            (0, 0, -1),
            (123, -456, 789),
            (-100_000, 100_000, -50_000),
            (CHUNK_MORTON_BIAS - 1, CHUNK_MORTON_BIAS - 1, CHUNK_MORTON_BIAS - 1),
            (-CHUNK_MORTON_BIAS, -CHUNK_MORTON_BIAS, -CHUNK_MORTON_BIAS),
        ];
        for (cx, cy, cz) in cases {
            let m = encode_chunk_morton(cx, cy, cz);
            let (rx, ry, rz) = decode_chunk_morton(m);
            assert_eq!(
                (rx, ry, rz),
                (cx, cy, cz),
                "round-trip failed for ({cx},{cy},{cz}) (morton=0x{m:016x})",
            );
        }
    }

    #[test]
    #[should_panic]
    fn out_of_range_panics() {
        encode_chunk_morton(CHUNK_MORTON_BIAS, 0, 0);
    }

    #[test]
    fn zyx_ordering_locked() {
        // (1,0,0) → bit 0 of x  → output bit 0 → 0b001 = 1
        // (0,1,0) → bit 0 of y  → output bit 1 → 0b010 = 2
        // (0,0,1) → bit 0 of z  → output bit 2 → 0b100 = 4
        // … plus the bias terms (bias contributes equally on all three axes
        // so subtracting two encoded values cancels the bias).
        let base = encode_chunk_morton(0, 0, 0);
        assert_eq!(encode_chunk_morton(1, 0, 0) - base, 0b001);
        assert_eq!(encode_chunk_morton(0, 1, 0) - base, 0b010);
        assert_eq!(encode_chunk_morton(0, 0, 1) - base, 0b100);
        // bit-2 of x lands at output bit 6 (3*2)
        assert_eq!(encode_chunk_morton(4, 0, 0) - base, 1 << 6);
    }

    #[test]
    fn voxel_idx_layout() {
        assert_eq!(voxel_idx(0, 0, 0, 32), 0);
        assert_eq!(voxel_idx(1, 0, 0, 32), 1);
        assert_eq!(voxel_idx(0, 1, 0, 32), 32);
        assert_eq!(voxel_idx(0, 0, 1, 32), 32 * 32);
        assert_eq!(voxel_idx(31, 31, 31, 32), 32 * 32 * 32 - 1);
    }
}
