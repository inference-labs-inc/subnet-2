pub fn grid_reconstruct(
    tiles: &[&[f64]],
    tiles_y: usize,
    tiles_x: usize,
    channels: usize,
    tile_h: usize,
    tile_w: usize,
) -> Vec<f64> {
    let out_h = tiles_y * tile_h;
    let out_w = tiles_x * tile_w;
    let mut output = vec![0.0f64; channels * out_h * out_w];

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

    output
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

        let result = grid_reconstruct(&tiles, 2, 2, 1, 2, 2);

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

        let result = grid_reconstruct(&tiles, 2, 2, c, h, w);

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
        let result = grid_reconstruct(&tiles, 1, 1, 2, 1, 3);
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
        let result = grid_reconstruct(&tiles, 3, 2, c, h, w);
        assert_eq!(result.len(), c * 6 * 6);
    }
}
