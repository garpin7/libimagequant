use crate::error::*;
use crate::image::Image;
use crate::kmeans::Kmeans;
use crate::nearest::Nearest;
use crate::pal::{ARGBF, LIQ_WEIGHT_MSE, MIN_OPAQUE_A, PalF, PalIndex, Palette, f_pixel, gamma_lut};
use crate::quant::{quality_to_mse, QuantizationResult};
use crate::rows::temp_buf;
use crate::seacow::{RowBitmap, RowBitmapMut};
use rayon::iter::ParallelBridge;
use rayon::iter::ParallelIterator;
use rgb::ComponentMap;
use std::cell::RefCell;
use std::mem::MaybeUninit;
use thread_local::ThreadLocal;

#[repr(u8)]
#[derive(Eq, PartialEq, Clone, Copy)]
pub enum DitherMapMode {
    None = 0,
    Enabled = 1,
    Always = 2,
}

pub(crate) struct Remapped {
    pub(crate) int_palette: Palette,
    pub(crate) palette_error: Option<f64>,
}

#[inline(never)]
pub(crate) fn remap_to_palette<'x, 'b: 'x>(image: &mut Image, output_pixels: &'x mut RowBitmapMut<'b, MaybeUninit<u8>>, palette: &mut PalF) -> Result<(f64, RowBitmap<'x, u8>), liq_error> {
    let width = image.width();

    let n = Nearest::new(palette);
    let colors = palette.as_slice();
    let palette_len = colors.len();

    let mut background = image.background.as_mut();
    let transparent_index = if background.is_some() {
        n.search(&f_pixel::default(), 0).0 as i16
    } else { -1 };

    if background.is_some() && colors[transparent_index as usize].a > MIN_OPAQUE_A {
        background = None;
    }

    let tls = ThreadLocal::new();
    let per_thread_buffers = move || (RefCell::new((Kmeans::new(palette_len), temp_buf(width), temp_buf(width), temp_buf(width))));

    let tls_tmp1 = tls.get_or(per_thread_buffers);
    let mut tls_tmp = tls_tmp1.borrow_mut();

    let input_rows = image.px.rows_iter(&mut tls_tmp.1)?;
    let background = background.map(|bg| bg.px.rows_iter(&mut tls_tmp.1)).transpose()?;
    drop(tls_tmp);

    let remapping_error = output_pixels.rows_mut().enumerate().par_bridge().map(|(row, output_pixels_row)| {
        let mut remapping_error = 0.;
        let (kmeans, temp_row, temp_row_f, temp_row_f_bg) = &mut *tls.get_or(per_thread_buffers).borrow_mut();

        let output_pixels_row = &mut output_pixels_row[..width];
        let row_pixels = &input_rows.row_f2(temp_row, temp_row_f, row)[..width];
        let bg_pixels = if let Some(background) = &background  {
            &background.row_f2(temp_row, temp_row_f_bg, row)[..width]
        } else { &[] };

        let mut last_match = 0;
        for (col, (inp, out)) in row_pixels.iter().zip(output_pixels_row).enumerate() {
            let (idx, mut diff) = n.search(inp, last_match);
            last_match = idx;
            if !bg_pixels.is_empty() {
                let bg_diff = bg_pixels[col].diff(&colors[last_match as usize]);
                if bg_diff <= diff {
                    diff = bg_diff;
                    last_match = transparent_index as PalIndex;
                }
            }
            out.write(last_match);
            remapping_error += diff as f64;
            if last_match as i16 != transparent_index {
                kmeans.update_color(*inp, 1., last_match);
            }
        }
        remapping_error
    })
    .sum::<f64>();

    if let Some(kmeans) = tls.into_iter()
        .map(|t| RefCell::into_inner(t).0)
        .reduce(Kmeans::merge) { kmeans.finalize(palette); }

    let remapping_error = remapping_error / (image.px.width * image.px.height) as f64;
    Ok((remapping_error, unsafe { output_pixels.assume_init() }))
}

fn get_dithered_pixel(dither_level: f32, max_dither_error: f32, thiserr: f_pixel, px: f_pixel) -> f_pixel {
    let s = thiserr.0 * dither_level;
    // This prevents gaudy green pixels popping out of the blue (or red or black! ;)
    let dither_error = s.r * s.r + s.g * s.g + s.b * s.b + s.a * s.a;
    if dither_error < 2. / 256. / 256. {
        // don't dither areas that don't have noticeable error — makes file smaller
        return px;
    }

    let mut ratio: f32 = 1.;
    const MAX_OVERFLOW: f32 = 1.1;
    const MAX_UNDERFLOW: f32 = -0.1;
    // allowing some overflow prevents undithered bands caused by clamping of all channels
    if px.r + s.r > MAX_OVERFLOW {
        ratio = ratio.min((MAX_OVERFLOW - px.r) / s.r);
    } else if px.r + s.r < MAX_UNDERFLOW {
        ratio = ratio.min((MAX_UNDERFLOW - px.r) / s.r);
    }
    if px.g + s.g > MAX_OVERFLOW {
        ratio = ratio.min((MAX_OVERFLOW - px.g) / s.g);
    } else if px.g + s.g < MAX_UNDERFLOW {
        ratio = ratio.min((MAX_UNDERFLOW - px.g) / s.g);
    }
    if px.b + s.b > MAX_OVERFLOW {
        ratio = ratio.min((MAX_OVERFLOW - px.b) / s.b);
    } else if px.b + s.b < MAX_UNDERFLOW {
        ratio = ratio.min((MAX_UNDERFLOW - px.b) / s.b);
    }
    if dither_error > max_dither_error {
        ratio *= 0.8;
    }
    f_pixel(ARGBF {
        a: (px.a + s.a).clamp(0., 1.),
        r: px.r + s.r * ratio,
        g: px.g + s.g * ratio,
        b: px.b + s.b * ratio,
    })
}

/// Uses edge/noise map to apply dithering only to flat areas. Dithering on edges creates jagged lines, and noisy areas are "naturally" dithered.
///
///  If output_image_is_remapped is true, only pixels noticeably changed by error diffusion will be written to output image.
#[inline(never)]
pub(crate) fn remap_to_palette_floyd(input_image: &mut Image, mut output_pixels: RowBitmapMut<'_, MaybeUninit<u8>>, quant: &QuantizationResult, max_dither_error: f32, output_image_is_remapped: bool) -> Result<(), liq_error> {
    let progress_stage1 = if quant.use_dither_map != DitherMapMode::None { 20 } else { 0 };

    let width = input_image.width();
    let height = input_image.height();

    let mut temp_row = temp_buf(width);

    let dither_map = if quant.use_dither_map != DitherMapMode::None {
        input_image.dither_map.as_deref().or(input_image.edges.as_deref()).unwrap_or(&[])
    } else {
        &[]
    };
    let mut input_image_iter = input_image.px.rows_iter(&mut temp_row)?;
    let mut background = input_image.background.as_mut().map(|bg| bg.px.rows_iter(&mut temp_row)).transpose()?;

    let errwidth = width + 2; // +2 saves from checking out of bounds access
    let mut thiserr_data = vec![f_pixel::default(); errwidth * 2];
    let (mut thiserr, mut nexterr) = thiserr_data.split_at_mut(errwidth);
    let n = Nearest::new(&quant.palette);
    let palette = quant.palette.as_slice();

    let transparent_index = if background.is_some() { n.search(&f_pixel::default(), 0).0 } else { 0 };
    if background.is_some() && palette[transparent_index as usize].a > MIN_OPAQUE_A {
        background = None;
    }
    // response to this value is non-linear and without it any value < 0.8 would give almost no dithering
    let mut base_dithering_level = (1. - (1. - quant.dither_level) * (1. - quant.dither_level)) * (15. / 16.); // prevent small errors from accumulating
    if !dither_map.is_empty() {
        base_dithering_level *= 1. / 255.; // dither_map is in 0-255 scale
    }
    let mut scan_forward = true;
    let mut temp_row = temp_buf(width);

    for (row, output_pixels_row) in output_pixels.rows_mut().enumerate() {
        if quant.remap_progress(progress_stage1 as f32 + row as f32 * (100. - progress_stage1 as f32) / height as f32) {
            return Err(LIQ_ABORTED);
        }
        nexterr.fill_with(f_pixel::default);
        let mut col = if scan_forward { 0 } else { width - 1 };
        let row_pixels = input_image_iter.row_f(&mut temp_row, row as _);
        let bg_pixels = background.as_mut().map(|b| b.row_f(&mut temp_row, row as _)).unwrap_or(&[]);
        let dither_map = dither_map.get(row * width .. row * width + width).unwrap_or(&[]);
        let mut undithered_bg_used = 0;
        let mut last_match = 0;
        loop {
            let mut dither_level = base_dithering_level;
            if let Some(&l) = dither_map.get(col) {
                dither_level *= l as f32;
            }
            let input_px = row_pixels[col];
            let spx = get_dithered_pixel(dither_level, max_dither_error, thiserr[col + 1], input_px);
            let guessed_match = if output_image_is_remapped {
                unsafe { output_pixels_row[col].assume_init() }
            } else {
                last_match
            };
            let (dither_index, dither_diff) = n.search(&spx, guessed_match);
            last_match = dither_index;
            let mut output_px = palette[last_match as usize];
            if let Some(bg_pixel) = bg_pixels.get(col) {
                // if the background makes better match *with* dithering, it's a definitive win
                let bg_for_dither_diff = spx.diff(bg_pixel);
                if bg_for_dither_diff <= dither_diff {
                    output_px = *bg_pixel;
                    last_match = transparent_index;
                } else if undithered_bg_used > 1 {
                    // the undithered fallback can cause artifacts when too many undithered pixels accumulate a big dithering error
                    // so periodically ignore undithered fallback to prevent that
                    undithered_bg_used = 0;
                } else {
                    // if dithering is not applied, there's a high risk of creating artifacts (flat areas, error accumulating badly),
                    // OTOH poor dithering disturbs static backgrounds and creates oscilalting frames that break backgrounds
                    // back and forth in two differently bad ways
                    let max_diff = input_px.diff(bg_pixel);
                    let dithered_diff = input_px.diff(&output_px);
                    // if dithering is worse than natural difference between frames
                    // (this rule dithers moving areas, but does not dither static areas)
                    if dithered_diff > max_diff {
                        // then see if an undithered color is closer to the ideal
                        let guessed_px = palette[guessed_match as usize];
                        let undithered_diff = input_px.diff(&guessed_px); // If dithering error is crazy high, don't propagate it that much
                        if undithered_diff < max_diff {
                            undithered_bg_used += 1;
                            output_px = guessed_px;
                            last_match = guessed_match;
                        }
                    }
                }
            }
            output_pixels_row[col].write(last_match);
            let mut err = spx.0 - output_px.0;
            // This prevents crazy geen pixels popping out of the blue (or red or black! ;)
            if err.r * err.r + err.g * err.g + err.b * err.b + err.a * err.a > max_dither_error {
                err *= 0.75;
            }
            if scan_forward {
                thiserr[col + 2].0 += err * (7. / 16.);
                nexterr[col + 2].0 = err * (1. / 16.);
                nexterr[col + 1].0 += err * (5. / 16.);
                nexterr[col].0 += err * (3. / 16.);
            } else {
                thiserr[col].0 += err * (7. / 16.);
                nexterr[col + 2].0 += err * (3. / 16.);
                nexterr[col + 1].0 += err * (5. / 16.);
                nexterr[col].0 = err * (1. / 16.);
            }
            if scan_forward {
                col += 1;
                if col >= width {
                    break;
                }
            } else {
                if col == 0 {
                    break;
                }
                col -= 1;
            }
        }
        std::mem::swap(&mut thiserr, &mut nexterr);
        scan_forward = !scan_forward;
    }
    Ok(())
}

impl Remapped {
    #[allow(clippy::or_fun_call)]
    pub fn new(result: &QuantizationResult, image: &mut Image, mut output_pixels: RowBitmapMut<'_, MaybeUninit<u8>>) -> Result<Self, liq_error> {
        let mut palette = result.palette.clone();
        let progress_stage1 = if result.use_dither_map != DitherMapMode::None { 20 } else { 0 };

        let posterize = result.min_posterization_output;
        if result.remap_progress(progress_stage1 as f32 * 0.25) {
            return Err(LIQ_ABORTED);
        }

        let mut palette_error = result.palette_error;
        let int_palette;
        if result.dither_level == 0. {
            int_palette = Self::make_int_palette(&mut palette, result.gamma, posterize);
            palette_error = Some(remap_to_palette(image, &mut output_pixels, &mut palette)?.0);
        } else {
            let is_image_huge = (image.px.width * image.px.height) > 2000 * 2000;
            let allow_dither_map = result.use_dither_map == DitherMapMode::Always || (!is_image_huge && result.use_dither_map != DitherMapMode::None);
            let generate_dither_map = allow_dither_map && (image.edges.is_some() && image.dither_map.is_none());
            if generate_dither_map {
                // If dithering (with dither map) is required, this image is used to find areas that require dithering
                let (tmp_re, row_pointers_remapped) = remap_to_palette(image, &mut output_pixels, &mut palette)?;
                palette_error = Some(tmp_re);
                image.update_dither_map(&row_pointers_remapped, &mut palette);
            }
            let output_image_is_remapped = generate_dither_map;

            if result.remap_progress(progress_stage1 as f32 * 0.5) {
                return Err(LIQ_ABORTED);
            }

            // remapping above was the last chance to do K-Means iteration, hence the final palette is set after remapping
            int_palette = Self::make_int_palette(&mut palette, result.gamma, posterize);
            let max_dither_error = (palette_error.unwrap_or(quality_to_mse(80)) * 2.4).max(quality_to_mse(35)) as f32;
            remap_to_palette_floyd(image, output_pixels, result, max_dither_error, output_image_is_remapped)?;
        }

        Ok(Self {
            int_palette, palette_error,
        })
    }

    /// Also rounds the input pal
    pub fn make_int_palette(palette: &mut PalF, gamma: f64, posterize: u8) -> Palette {
        let mut int_palette = Palette {
            count: palette.len() as _,
            entries: [Default::default(); 256],
        };
        let lut = gamma_lut(gamma);
        for ((f_color, f_pop), int_pal) in palette.iter_mut().zip(int_palette.as_mut_slice()) {
            let mut px = f_color.to_rgb(gamma)
                .map(move |c| posterize_channel(c, posterize));
            *f_color = f_pixel::from_rgba(&lut, px);
            if px.a == 0 && !f_pop.is_fixed() {
                px.r = 71u8;
                px.g = 112u8;
                px.b = 76u8;
            }
            *int_pal = px;
        }
        int_palette
    }
}

pub(crate) fn mse_to_standard_mse(mse: f64) -> f64 {
    (mse * 65536. / 6.) / LIQ_WEIGHT_MSE // parallelized dither map might speed up floyd remapping
}

#[inline]
fn posterize_channel(color: u8, bits: u8) -> u8 {
    if bits == 0 {
        color
    } else {
        (color & !((1 << bits) - 1)) | (color >> (8 - bits))
    }
}

#[test]
fn send() {
    fn is_send<T: Send>() {}

    is_send::<RowBitmapMut<'_, MaybeUninit<u8>>>();
}
