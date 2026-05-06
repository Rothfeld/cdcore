//! CLIP image preprocessing.
//!
//! Pipeline from the OpenAI CLIP repo:
//!   1. resize so shorter side = image_size (CatmullRom = bicubic-ish)
//!   2. center crop to image_size x image_size
//!   3. normalize per-channel with CLIP mean/std
//!   4. CHW float32 tensor

use candle_core::{Device, Tensor};
use image::imageops::FilterType;

use crate::error::Result;

const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

pub fn load_clip_image(path: &std::path::Path, image_size: u32) -> Result<Tensor> {
    let img = image::open(path)?.to_rgb8();
    let (w, h) = (img.width(), img.height());
    assert!(w > 0 && h > 0, "zero-sized image: {}", path.display());
    rgb_to_tensor(img, w, h, image_size)
}

/// Same pipeline as `load_clip_image` but starts from already-decoded RGBA
/// bytes (e.g. from `cdcore::formats::image::dds::decode_dds_to_rgba`).
pub fn load_clip_image_from_rgba(
    rgba: &[u8],
    w: u32,
    h: u32,
    image_size: u32,
) -> Result<Tensor> {
    assert!(w > 0 && h > 0, "zero-sized rgba: {w}x{h}");
    assert_eq!(
        rgba.len(),
        (w as usize) * (h as usize) * 4,
        "rgba length {} != w*h*4 ({}x{}*4)",
        rgba.len(), w, h
    );
    // Drop alpha by copying R, G, B; ignore A entirely.
    let mut rgb = image::RgbImage::new(w, h);
    for (i, px) in rgb.pixels_mut().enumerate() {
        let o = i * 4;
        *px = image::Rgb([rgba[o], rgba[o + 1], rgba[o + 2]]);
    }
    rgb_to_tensor(rgb, w, h, image_size)
}

fn rgb_to_tensor(img: image::RgbImage, w: u32, h: u32, image_size: u32) -> Result<Tensor> {
    let scale = image_size as f32 / w.min(h) as f32;
    let nw = (w as f32 * scale).round() as u32;
    let nh = (h as f32 * scale).round() as u32;
    let resized = image::imageops::resize(&img, nw, nh, FilterType::CatmullRom);
    let x0 = (nw - image_size) / 2;
    let y0 = (nh - image_size) / 2;
    let cropped =
        image::imageops::crop_imm(&resized, x0, y0, image_size, image_size).to_image();

    let s = image_size as usize;
    let mut data = vec![0.0f32; 3 * s * s];
    for (y, row) in cropped.rows().enumerate() {
        for (x, px) in row.enumerate() {
            for c in 0..3 {
                let v = px.0[c] as f32 / 255.0;
                data[c * s * s + y * s + x] = (v - MEAN[c]) / STD[c];
            }
        }
    }
    Ok(Tensor::from_vec(data, (3, s, s), &Device::Cpu)?)
}
