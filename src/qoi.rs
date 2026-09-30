//! A QOI decoder, for the reveal image
//!
//! QOI because it decodes in one pass with no tables, no entropy coding and no
//! allocation beyond the output, at several hundred MB/s - a wallpaper-sized
//! image in a few milliseconds at startup, where a JPEG or PNG decoder would
//! also cost a dependency. cavawall-tune writes it; the browser does all the
//! decoding of real formats. Spec: <https://qoiformat.org/qoi-specification.pdf>

/// Width and height past this are refused: nothing on a real monitor needs
/// more, and a corrupt header must not ask for gigabytes
pub const MAX_SIDE: u32 = 8192;

const OP_RGB: u8 = 0xfe;
const OP_RGBA: u8 = 0xff;
const MASK: u8 = 0xc0;
const OP_INDEX: u8 = 0x00;
const OP_DIFF: u8 = 0x40;
const OP_LUMA: u8 = 0x80;
const OP_RUN: u8 = 0xc0;

/// An RGBA8 image, rows top to bottom
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[inline]
fn slot(px: [u8; 4]) -> usize {
    let [r, g, b, a] = px.map(usize::from);
    (r * 3 + g * 5 + b * 7 + a * 11) % 64
}

/// Decode a whole file. None on anything malformed: a truncated stream, a bad
/// header, or a size past [`MAX_SIDE`]
#[must_use]
pub fn decode(data: &[u8]) -> Option<Image> {
    let header = data.get(..14)?;
    if &header[..4] != b"qoif" {
        return None;
    }
    let side = |at: usize| u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    let (width, height) = (side(4), side(8));
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let pixels = width as usize * height as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    let mut index = [[0u8; 4]; 64];
    let mut px = [0u8, 0, 0, 255];
    let mut run = 0usize;
    let mut at = 14;
    let body = data.get(..data.len().checked_sub(8)?)?;
    for _ in 0..pixels {
        if run > 0 {
            run -= 1;
        } else {
            let op = *body.get(at)?;
            at += 1;
            match op {
                OP_RGB => {
                    px[..3].copy_from_slice(body.get(at..at + 3)?);
                    at += 3;
                }
                OP_RGBA => {
                    px.copy_from_slice(body.get(at..at + 4)?);
                    at += 4;
                }
                _ => match op & MASK {
                    OP_INDEX => px = index[usize::from(op)],
                    OP_DIFF => {
                        px[0] = px[0].wrapping_add((op >> 4 & 3).wrapping_sub(2));
                        px[1] = px[1].wrapping_add((op >> 2 & 3).wrapping_sub(2));
                        px[2] = px[2].wrapping_add((op & 3).wrapping_sub(2));
                    }
                    OP_LUMA => {
                        let next = *body.get(at)?;
                        at += 1;
                        let dg = (op & 0x3f).wrapping_sub(32);
                        px[0] = px[0].wrapping_add(dg.wrapping_add((next >> 4).wrapping_sub(8)));
                        px[1] = px[1].wrapping_add(dg);
                        px[2] = px[2].wrapping_add(dg.wrapping_add((next & 0x0f).wrapping_sub(8)));
                    }
                    // OP_RUN: this pixel and `run` more
                    _ => {
                        debug_assert_eq!(op & MASK, OP_RUN);
                        run = usize::from(op & 0x3f);
                    }
                },
            }
            index[slot(px)] = px;
        }
        rgba.extend_from_slice(&px);
    }
    Some(Image { width, height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A straightforward encoder, the reference's structure, so the decoder is
    /// tested against every op rather than against itself
    fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = b"qoif".to_vec();
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[4, 0]);
        let (mut index, mut prev, mut run) = ([[0u8; 4]; 64], [0u8, 0, 0, 255], 0u8);
        let pixels: &[[u8; 4]] = rgba.as_chunks::<4>().0;
        for (i, &px) in pixels.iter().enumerate() {
            if px == prev {
                run += 1;
                if run == 62 || i == pixels.len() - 1 {
                    out.push(OP_RUN | (run - 1));
                    run = 0;
                }
                continue;
            }
            if run > 0 {
                out.push(OP_RUN | (run - 1));
                run = 0;
            }
            let s = slot(px);
            if index[s] == px {
                out.push(OP_INDEX | s as u8);
            } else {
                index[s] = px;
                if px[3] == prev[3] {
                    let d = |a: u8, b: u8| a.wrapping_sub(b) as i8;
                    let (dr, dg, db) = (d(px[0], prev[0]), d(px[1], prev[1]), d(px[2], prev[2]));
                    let (dr_dg, db_dg) = (dr.wrapping_sub(dg), db.wrapping_sub(dg));
                    if (-2..=1).contains(&dr) && (-2..=1).contains(&dg) && (-2..=1).contains(&db) {
                        out.push(OP_DIFF | ((dr + 2) as u8) << 4 | ((dg + 2) as u8) << 2 | (db + 2) as u8);
                    } else if (-32..=31).contains(&dg) && (-8..=7).contains(&dr_dg) && (-8..=7).contains(&db_dg) {
                        out.push(OP_LUMA | (dg + 32) as u8);
                        out.push(((dr_dg + 8) as u8) << 4 | (db_dg + 8) as u8);
                    } else {
                        out.extend_from_slice(&[OP_RGB, px[0], px[1], px[2]]);
                    }
                } else {
                    out.extend_from_slice(&[OP_RGBA, px[0], px[1], px[2], px[3]]);
                }
            }
            prev = px;
        }
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
        out
    }

    #[test]
    fn every_op_round_trips() {
        // Runs, near neighbours (DIFF), mid-range steps (LUMA), repeats of an
        // older colour (INDEX), big jumps (RGB) and alpha changes (RGBA)
        let mut rgba = Vec::new();
        for i in 0u32..300 {
            let v = (i * 7 % 256) as u8;
            let px = match i % 6 {
                2 => [11, 21, 29, 255],
                3 => [v, v.wrapping_add(9), v.wrapping_add(3), 255],
                4 => [v, 255 - v, v / 2, (i % 256) as u8],
                _ => [10, 20, 30, 255],
            };
            rgba.extend_from_slice(&px);
        }
        let img = decode(&encode(20, 15, &rgba)).expect("decodes");
        assert_eq!((img.width, img.height), (20, 15));
        assert_eq!(img.rgba, rgba);
    }

    #[test]
    fn a_long_run_spans_several_run_ops() {
        let rgba = [7u8, 8, 9, 255].repeat(200);
        assert_eq!(decode(&encode(10, 20, &rgba)).expect("decodes").rgba, rgba);
    }

    #[test]
    fn malformed_input_is_refused_not_trusted() {
        let good = encode(4, 4, &[1u8, 2, 3, 255].repeat(16));
        assert!(decode(&good[..good.len() - 12]).is_none(), "truncated");
        let mut bad = good.clone();
        bad[0] = b'x';
        assert!(decode(&bad).is_none(), "magic");
        let mut huge = good;
        huge[4..8].copy_from_slice(&(MAX_SIDE + 1).to_be_bytes());
        assert!(decode(&huge).is_none(), "size bound");
        assert!(decode(&[]).is_none());
    }
}
