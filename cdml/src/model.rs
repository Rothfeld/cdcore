//! Load CLIP fp32 weights from HuggingFace and run image/text encoding on CPU.

use std::path::PathBuf;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::clip::{
    text_model::{Activation as ClipActivation, ClipTextConfig},
    vision_model::ClipVisionConfig,
    ClipConfig, ClipModel,
};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;

use crate::error::Result;
use crate::preprocess;

#[derive(Debug, Clone, Copy)]
pub enum ClipVariant {
    /// openai/clip-vit-base-patch32 -- 512-dim, 224 px, ~600 MB.
    ViTBase32,
}

impl ClipVariant {
    pub fn repo(self) -> &'static str {
        match self {
            ClipVariant::ViTBase32 => "openai/clip-vit-base-patch32",
        }
    }

    pub fn config(self) -> ClipConfig {
        match self {
            ClipVariant::ViTBase32 => ClipConfig {
                text_config: ClipTextConfig {
                    vocab_size: 49408,
                    embed_dim: 512,
                    activation: ClipActivation::QuickGelu,
                    intermediate_size: 2048,
                    max_position_embeddings: 77,
                    pad_with: None,
                    num_hidden_layers: 12,
                    num_attention_heads: 8,
                    projection_dim: 512,
                },
                vision_config: ClipVisionConfig::vit_base_patch32(),
                logit_scale_init_value: 2.6592,
                image_size: 224,
            },
        }
    }

    pub fn image_size(self) -> u32 {
        self.config().image_size as u32
    }

    pub fn embed_dim(self) -> usize {
        self.config().text_config.projection_dim
    }
}

impl std::str::FromStr for ClipVariant {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "base32" | "vit-base-patch32" => Ok(ClipVariant::ViTBase32),
            o => Err(format!("unknown clip variant {o:?}; only base32 currently")),
        }
    }
}

pub struct Clip {
    pub variant: ClipVariant,
    pub device: Device,
    pub tokenizer: Tokenizer,
    pub max_text_tokens: usize,
    model: ClipModel,
}

impl Clip {
    /// Pick the best available device: CUDA:0 if a GPU is present, else CPU.
    pub fn best_device() -> Device {
        match Device::cuda_if_available(0) {
            Ok(d) => d,
            Err(_) => Device::Cpu,
        }
    }

    pub fn load(variant: ClipVariant, device: Device) -> Result<Self> {
        let api = Api::new()?;
        let repo = api.model(variant.repo().to_string());
        let weights = repo.get("pytorch_model.bin")?;
        let tokenizer_path = repo.get("tokenizer.json")?;

        let cfg = variant.config();
        let max_text_tokens = cfg.text_config.max_position_embeddings;

        let vb = VarBuilder::from_pth(&weights, DType::F32, &device)?;
        let model = ClipModel::new(vb, &cfg)?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| crate::error::CdmlError::Tokenizer(e.to_string()))?;
        let pad_id = tokenizer.token_to_id("<|endoftext|>").unwrap_or(0);
        let _ = tokenizer.with_padding(Some(tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::Fixed(max_text_tokens),
            direction: tokenizers::PaddingDirection::Right,
            pad_id,
            pad_token: "<|endoftext|>".to_string(),
            pad_type_id: 0,
            pad_to_multiple_of: None,
        }));
        let _ = tokenizer.with_truncation(Some(tokenizers::TruncationParams {
            max_length: max_text_tokens,
            ..Default::default()
        }));

        Ok(Self { variant, device, tokenizer, max_text_tokens, model })
    }

    /// Encode a batch of pre-decoded RGBA bytes (e.g. from a DDS reader).
    /// Each item is `(rgba_bytes, width, height)`. Preprocessing parallelized
    /// across rayon's global thread pool; the GPU forward stays serial.
    pub fn embed_rgba_batch(&self, items: &[(Vec<u8>, u32, u32)]) -> Result<Vec<Vec<f32>>> {
        use rayon::prelude::*;
        if items.is_empty() {
            return Ok(vec![]);
        }
        let image_size = self.variant.image_size();
        let tensors: Vec<Tensor> = items
            .par_iter()
            .map(|(rgba, w, h)| preprocess::load_clip_image_from_rgba(rgba, *w, *h, image_size))
            .collect::<Result<Vec<_>>>()?;
        let batch = Tensor::stack(&tensors, 0)?.to_device(&self.device)?;
        let feats = self.model.get_image_features(&batch)?;
        let normed = l2_normalize(&feats)?;
        let dim = self.variant.embed_dim();
        let n = items.len();
        let flat: Vec<f32> = normed.flatten_all()?.to_vec1::<f32>()?;
        assert_eq!(flat.len(), n * dim, "embed_rgba_batch: shape != n*dim");
        Ok(flat.chunks(dim).map(|c| c.to_vec()).collect())
    }

    pub fn embed_image(&self, path: &std::path::Path) -> Result<Vec<f32>> {
        let img = preprocess::load_clip_image(path, self.variant.image_size())?;
        let batch = img.unsqueeze(0)?.to_device(&self.device)?;
        let feats = self.model.get_image_features(&batch)?;
        let normed = l2_normalize(&feats)?;
        let v = normed.flatten_all()?.to_vec1::<f32>()?;
        debug_assert_eq!(v.len(), self.variant.embed_dim());
        Ok(v)
    }

    pub fn embed_images(&self, paths: &[PathBuf]) -> Result<Vec<Vec<f32>>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        let mut tensors = Vec::with_capacity(paths.len());
        for p in paths {
            tensors.push(preprocess::load_clip_image(p, self.variant.image_size())?);
        }
        let batch = Tensor::stack(&tensors, 0)?.to_device(&self.device)?;
        let feats = self.model.get_image_features(&batch)?;
        let normed = l2_normalize(&feats)?;
        let dim = self.variant.embed_dim();
        let n = paths.len();
        let flat: Vec<f32> = normed.flatten_all()?.to_vec1::<f32>()?;
        assert_eq!(flat.len(), n * dim, "embed_images: tensor shape != n*dim");
        Ok(flat.chunks(dim).map(|c| c.to_vec()).collect())
    }

    pub fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        let enc = self.tokenizer.encode(text, true)?;
        let ids: Vec<u32> = enc.get_ids().to_vec();
        assert_eq!(
            ids.len(),
            self.max_text_tokens,
            "tokenizer should pad to {}",
            self.max_text_tokens
        );
        let ids_t = Tensor::new(ids.as_slice(), &self.device)?.unsqueeze(0)?;
        let feats = self.model.get_text_features(&ids_t)?;
        let normed = l2_normalize(&feats)?;
        let v = normed.flatten_all()?.to_vec1::<f32>()?;
        debug_assert_eq!(v.len(), self.variant.embed_dim());
        Ok(v)
    }
}

fn l2_normalize(t: &Tensor) -> Result<Tensor> {
    let norm = t.sqr()?.sum_keepdim(1)?.sqrt()?;
    Ok(t.broadcast_div(&norm)?)
}
