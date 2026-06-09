use anyhow::{bail, Result};

const MAX_OUTPUT_ELEMENTS: usize = 1 << 26;

pub fn grid_reconstruct(
    tiles: &[&[f64]],
    tiles_y: usize,
    tiles_x: usize,
    channels: usize,
    tile_h: usize,
    tile_w: usize,
) -> Result<Vec<f64>> {
    let expected_tiles = tiles_y
        .checked_mul(tiles_x)
        .ok_or_else(|| anyhow::anyhow!("tile grid {tiles_y}x{tiles_x} overflows"))?;
    if tiles.len() != expected_tiles {
        bail!("tile count {} != grid {tiles_y}x{tiles_x}", tiles.len());
    }
    let tile_elements = channels
        .checked_mul(tile_h)
        .and_then(|v| v.checked_mul(tile_w))
        .ok_or_else(|| anyhow::anyhow!("tile shape {channels}x{tile_h}x{tile_w} overflows"))?;
    let out_h = tiles_y
        .checked_mul(tile_h)
        .ok_or_else(|| anyhow::anyhow!("output height {tiles_y}*{tile_h} overflows"))?;
    let out_w = tiles_x
        .checked_mul(tile_w)
        .ok_or_else(|| anyhow::anyhow!("output width {tiles_x}*{tile_w} overflows"))?;
    let total_elements = channels
        .checked_mul(out_h)
        .and_then(|v| v.checked_mul(out_w))
        .ok_or_else(|| anyhow::anyhow!("output shape {channels}x{out_h}x{out_w} overflows"))?;
    if total_elements > MAX_OUTPUT_ELEMENTS {
        bail!(
            "output shape {channels}x{out_h}x{out_w} has {total_elements} elements, max is {MAX_OUTPUT_ELEMENTS}"
        );
    }
    for (idx, tile) in tiles.iter().enumerate() {
        if tile.len() != tile_elements {
            bail!(
                "tile {idx} length {} != expected {tile_elements}",
                tile.len()
            );
        }
    }

    let mut output = vec![0.0f64; total_elements];

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let tile = tiles[ty * tiles_x + tx];
            for c in 0..channels {
                for y in 0..tile_h {
                    let src_offset = c * tile_h * tile_w + y * tile_w;
                    let dst_h = ty * tile_h + y;
                    let dst_w = tx * tile_w;
                    let dst_offset = c * out_h * out_w + dst_h * out_w + dst_w;
                    output[dst_offset..dst_offset + tile_w]
                        .copy_from_slice(&tile[src_offset..src_offset + tile_w]);
                }
            }
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_2x2_grid_single_channel() {
        let t0: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0];
        let t1: Vec<f64> = vec![5.0, 6.0, 7.0, 8.0];
        let t2: Vec<f64> = vec![9.0, 10.0, 11.0, 12.0];
        let t3: Vec<f64> = vec![13.0, 14.0, 15.0, 16.0];
        let tiles: Vec<&[f64]> = vec![&t0, &t1, &t2, &t3];

        let result = grid_reconstruct(&tiles, 2, 2, 1, 2, 2).unwrap();

        assert_eq!(result.len(), 4 * 4);
        #[rustfmt::skip]
        let expected = vec![
            1.0,  2.0,  5.0,  6.0,
            3.0,  4.0,  7.0,  8.0,
            9.0,  10.0, 13.0, 14.0,
            11.0, 12.0, 15.0, 16.0,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_2x2_grid_multi_channel() {
        let c = 3;
        let h = 2;
        let w = 2;
        let tile_size = c * h * w;

        let mut tiles_data: Vec<Vec<f64>> = Vec::new();
        for t in 0..4 {
            let mut tile = vec![0.0; tile_size];
            for ci in 0..c {
                for yi in 0..h {
                    for xi in 0..w {
                        tile[ci * h * w + yi * w + xi] = (t * 100 + ci * 10 + yi * w + xi) as f64;
                    }
                }
            }
            tiles_data.push(tile);
        }
        let tiles: Vec<&[f64]> = tiles_data.iter().map(|v| v.as_slice()).collect();

        let result = grid_reconstruct(&tiles, 2, 2, c, h, w).unwrap();

        assert_eq!(result.len(), c * 4 * 4);

        let out_h = 4;
        let out_w = 4;
        for ci in 0..c {
            for ty in 0..2usize {
                for tx in 0..2usize {
                    let tile_idx = ty * 2 + tx;
                    for yi in 0..h {
                        for xi in 0..w {
                            let expected_val = (tile_idx * 100 + ci * 10 + yi * w + xi) as f64;
                            let dst_y = ty * h + yi;
                            let dst_x = tx * w + xi;
                            let dst_idx = ci * out_h * out_w + dst_y * out_w + dst_x;
                            assert_eq!(
                                result[dst_idx], expected_val,
                                "mismatch at c={ci} y={dst_y} x={dst_x} (tile {tile_idx})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_1x1_grid() {
        let tile: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let tiles: Vec<&[f64]> = vec![&tile];
        let result = grid_reconstruct(&tiles, 1, 1, 2, 1, 3).unwrap();
        assert_eq!(result, tile);
    }

    #[test]
    fn test_3x2_grid() {
        let c = 1;
        let h = 2;
        let w = 3;
        let mut tiles_data: Vec<Vec<f64>> = Vec::new();
        for t in 0..6 {
            tiles_data.push((0..c * h * w).map(|i| (t * 100 + i) as f64).collect());
        }
        let tiles: Vec<&[f64]> = tiles_data.iter().map(|v| v.as_slice()).collect();
        let result = grid_reconstruct(&tiles, 3, 2, c, h, w).unwrap();
        assert_eq!(result.len(), c * 6 * 6);
    }

    #[test]
    fn test_rejects_output_above_element_cap() {
        let tile: Vec<f64> = vec![0.0; 4];
        let tiles: Vec<&[f64]> = vec![&tile];
        let result = grid_reconstruct(&tiles, 1, 1, 2, 8192, 8192);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("max is"), "unexpected error: {err}");
    }

    #[test]
    fn test_rejects_overflowing_shape_product() {
        let tile: Vec<f64> = vec![0.0; 4];
        let tiles: Vec<&[f64]> = vec![&tile];
        let result = grid_reconstruct(&tiles, 1, 1, usize::MAX, usize::MAX, usize::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_rejects_tile_count_mismatch() {
        let tile: Vec<f64> = vec![0.0; 4];
        let tiles: Vec<&[f64]> = vec![&tile];
        let result = grid_reconstruct(&tiles, 2, 2, 1, 2, 2);
        assert!(result.is_err());
    }

    #[test]
    fn test_rejects_tile_length_mismatch() {
        let short: Vec<f64> = vec![0.0; 3];
        let tiles: Vec<&[f64]> = vec![&short];
        let result = grid_reconstruct(&tiles, 1, 1, 1, 2, 2);
        assert!(result.is_err());
    }
}
