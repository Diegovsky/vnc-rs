use crate::{PixelFormat, Rect, VncError, VncImageEvent};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::error;

use super::uninit_vec;

async fn read_run_length<S>(reader: &mut S) -> Result<usize, VncError>
where
    S: AsyncRead + Unpin,
{
    let mut run_length_part;
    let mut run_length = 1;
    loop {
        run_length_part = reader.read_u8().await?;
        run_length += run_length_part as usize;
        if 255 != run_length_part {
            break;
        }
    }
    Ok(run_length)
}

async fn copy_true_color<S>(
    reader: &mut S,
    pixels: &mut Vec<u8>,
    pad: bool,
    compressed_bpp: usize,
    bpp: usize,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
{
    let mut buf = [255; 4];
    reader
        .read_exact(&mut buf[pad as usize..pad as usize + compressed_bpp])
        .await?;
    pixels.extend_from_slice(&buf[..bpp]);
    Ok(())
}

fn copy_indexed(palette: &[u8], pixels: &mut Vec<u8>, bpp: usize, index: u8) {
    let start = index as usize * bpp;
    pixels.extend_from_slice(&palette[start..start + bpp])
}

/// TRLE stands for Tiled Run-Length Encoding, and combines tiling, palettisation and run-length
/// encoding. The rectangle is divided into tiles of 16x16 pixels in left-to-right, top-to-bottom
/// order. (RFP Protocol specification)
///
/// It is a complex encoding because it combines a bunch of "sub-encodings".
pub struct Decoder {}

impl Decoder {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn decode<S>(
        &self,
        format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        frames: &mut Vec<VncImageEvent>
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
    {
        // The length part of the "Run-Length Encoding".
        let data_len = input.read_u32().await? as usize;

        // TODO: instead of always allocating, create
        // and reuse a Vec stored in the `Decoder struct`
        let mut zlib_data = uninit_vec(data_len);
        input.read_exact(&mut zlib_data).await?;

        let bpp = format.bits_per_pixel as usize / 8;
        let pixel_mask = (format.red_max as u32) << format.red_shift
            | (format.green_max as u32) << format.green_shift
            | (format.blue_max as u32) << format.blue_shift;

        let (compressed_bpp, alpha_at_first) =
            if format.bits_per_pixel == 32 && format.true_color_flag > 0 && format.depth <= 24 {
                if pixel_mask & 0x000000ff == 0 {
                    // rgb at the most significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data is expected to be [rgb.0, rgb.1, rgb.2, alpha]
                    // otherwise the decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    (3, format.big_endian_flag == 0)
                } else if pixel_mask & 0xff000000 == 0 {
                    // rgb at the least significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    // otherwise the decompressed data should be [rgb.0, rgb.1, rgb.2, alpha]
                    (3, format.big_endian_flag > 0)
                } else {
                    (4, false)
                }
            } else {
                (bpp, false)
            };
        // TODO: Do the same here.
        let mut palette = Vec::with_capacity(128 * bpp);

        let mut y = 0;
        while y < rect.height {
            let height = if y + 64 > rect.height {
                rect.height - y
            } else {
                64
            };
            let mut x = 0;
            while x < rect.width {
                let width = if x + 64 > rect.width {
                    rect.width - x
                } else {
                    64
                };
                let pixel_count = height as usize * width as usize;

                let control = input.read_u8().await?;
                let is_rle = control & 0x80 > 0;
                let palette_size = control & 0x7f;
                palette.truncate(0);

                for _ in 0..palette_size {
                    copy_true_color(
                        input,
                        &mut palette,
                        alpha_at_first,
                        compressed_bpp,
                        bpp,
                    ).await?
                }

                let mut pixels = Vec::with_capacity(pixel_count * bpp);
                match (is_rle, palette_size) {
                    (false, 0) => {
                        // True Color pixels
                        for _ in 0..pixel_count {
                            copy_true_color(
                                input,
                                &mut pixels,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            ).await?
                        }
                    }
                    (false, 1) => {
                        // Color fill
                        for _ in 0..pixel_count {
                            copy_indexed(&palette, &mut pixels, bpp, 0)
                        }
                    }
                    (false, 2..=16) => {
                        // Indexed pixels
                        let bits_per_index = match palette_size {
                            2 => 1,
                            3..=4 => 2,
                            5..=16 => 4,
                            _ => unreachable!(),
                        };
                        let mut encoded = input.read_u8().await?;
                        let mask = (1 << bits_per_index) - 1;

                        for y in 0..height {
                            let mut shift = 8 - bits_per_index;
                            for _ in 0..width {
                                if shift < 0 {
                                    shift = 8 - bits_per_index;
                                    encoded = input.read_u8().await?;
                                }
                                let idx = (encoded >> shift) & mask;

                                copy_indexed(&palette, &mut pixels, bpp, idx);
                                shift -= bits_per_index;
                            }
                            if shift < 8 - bits_per_index && y < height - 1 {
                                encoded = input.read_u8().await?;
                            }
                        }
                    }
                    (true, 0) => {
                        // True Color RLE
                        let mut count = 0;
                        let mut pixel = Vec::new();
                        while count < pixel_count {
                            pixel.truncate(0);
                            copy_true_color(
                                input,
                                &mut pixel,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            ).await?;
                            let run_length = read_run_length(input).await?;
                            for _ in 0..run_length {
                                pixels.extend(&pixel)
                            }
                            count += run_length;
                        }
                    }
                    (true, 2..=127) => {
                        // Indexed RLE
                        let mut count = 0;
                        while count < pixel_count {
                            let control = input.read_u8().await?;
                            let longer_than_one = control & 0x80 > 0;
                            let index = control & 0x7f;
                            let run_length = if longer_than_one {
                                read_run_length(input).await?
                            } else {
                                1
                            };
                            for _ in 0..run_length {
                                copy_indexed(&palette, &mut pixels, bpp, index);
                            }
                            count += run_length;
                        }
                    }
                    (x, y) => {
                        error!("TLRE subencoding error {:?}", (x, y));
                        return Err(VncError::InvalidImageData);
                    }
                }
                frames.push(VncImageEvent::RawImage(
                    crate::ImageData::new(Rect {
                        x: rect.x + x,
                        y: rect.y + y,
                        width,
                        height,
                    },
                    pixels)
                ));
                x += width;
            }
            y += height;
        }

        Ok(())
    }
}
