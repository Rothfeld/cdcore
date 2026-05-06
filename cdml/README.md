# cdml

CLIP image and text embeddings + brute-force vector search. Pure Rust (candle), CPU only, fp32.

Why fp32: on CPU candle, fp32 BLAS matmul is the fastest path. F16 falls back through f32-accumulate paths and runs ~3x slower; Q4K/Q6K/Q8_0 are tuned for LLM-scale matmuls and run 9-20x slower on CLIP-B/32's 512/2048-dim layers. Quantization wins on RAM, not on speed -- ship it later if RAM matters more than throughput.

## Oneliner

```bash
cargo run --release --bin cdml-index -- --images ./screenshots --out ./idx
cargo run --release --bin cdml-query -- --index ./idx --k 10 "red knight in snow"
```

## Numbers (CPU smoke test, ViT-B/32)

```
encode rate    : ~44 img/s
text encode    : ~37 ms
search latency : <1 ms (5 vecs); ~5-15 ms expected at 100k
RAM resident   : ~600 MB (model) + count*512*4 B (index)
top-1 recall   : "a red square" -> red_square.jpg
                 "deep blue color" -> blue_square.jpg
                 "yellow background with text" -> yellow_text.jpg
                 "a black circle on white background" -> white_circle.jpg
```

## Architecture

```
src/
  model.rs        Clip::load + embed_image / embed_images / embed_text (fp32, CPU)
  preprocess.rs   OpenAI CLIP image normalization (resize/crop/mean-std)
  index.rs        IndexWriter / IndexReader (raw f32 + paths.txt, mmap reads)
  search.rs       simsimd-backed cosine top-k over IndexReader
  bin/index.rs    cdml-index: walk folder, encode, write index
  bin/query.rs    cdml-query: text -> top-k images
```

## Index format

`embeddings.bin`:

```
[ "CDML" | u32 version=1 | u32 dim | u64 count | f32 le * count * dim ]
```

`paths.txt`: one UTF-8 path per line, exactly `count` lines.

Vectors are L2-normalized at write time; cosine search reduces to a dot product. Both files are mmap'd at query time. Brute-force scan via `simsimd`. At 100k * 512 dim, expect ~5-15 ms per query on one CPU core.

## Library use

```rust
use candle_core::Device;
use cdml::{Clip, ClipVariant, IndexWriter, IndexReader, topk};

let clip = Clip::load(ClipVariant::ViTBase32, Device::Cpu)?;

let mut w = IndexWriter::create(std::path::Path::new("./idx"), clip.variant.embed_dim())?;
for p in &paths {
    let v = clip.embed_image(p)?;
    w.push(&v, &p.to_string_lossy())?;
}
w.finish()?;

let r = IndexReader::open(std::path::Path::new("./idx"))?;
let q = clip.embed_text("a red square")?;
for hit in topk(&r, &q, 5)? {
    println!("{:.4}  {}", hit.score, hit.path);
}
```

## Notes

- `pytorch_model.bin` is the only weights file the upstream openai/clip repo ships; safetensors does not exist there. candle loads it via `VarBuilder::from_pth`. ~600 MB downloaded once to `~/.cache/huggingface/hub/`.
- `tokenizers` and `hf-hub` use pure-Rust feature variants (fancy-regex, rustls) so the build needs no openssl, esaxx-rs, or onig C deps.
- No HNSW/IVF index. At <5M vectors, brute force is faster to build, simpler to reason about, and lossless on recall. Add an index layer if/when that ceases to be true.
