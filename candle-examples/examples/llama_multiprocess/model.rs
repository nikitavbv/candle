use candle::backend::BackendStorage;
use candle::{CpuStorage, CustomOp1, DType, Device, IndexOp, Layout, Result, Shape, Tensor, D};
use candle_nn::{Embedding, Linear, Module, RmsNorm};
use cudarc::nccl::safe::{Comm, ReduceOp};
use half::f16;
use serde::Deserialize;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use super::MAX_SEQ_LEN;

use candle_nn::var_builder::ShardedVarBuilder as VarBuilder;

struct TensorParallelColumnLinear {
    linear: Linear,
}

impl TensorParallelColumnLinear {
    fn new(linear: Linear) -> Self {
        Self { linear }
    }
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        self.linear.forward(x)
    }
}

struct TensorParallelRowLinear {
    linear: Linear,
    comm: Rc<Comm>,
}

struct AllReduce {
    comm: Rc<Comm>,
}

/// This is actually not safe: https://docs.nvidia.com/deeplearning/nccl/user-guide/docs/usage/threadsafety.html
/// But for this example purposes, this will work
unsafe impl Sync for AllReduce {}
/// This is actually not safe: https://docs.nvidia.com/deeplearning/nccl/user-guide/docs/usage/threadsafety.html
/// But for this example purposes, this will work
unsafe impl Send for AllReduce {}

impl CustomOp1 for AllReduce {
    fn name(&self) -> &'static str {
        "allreduce"
    }

    fn cpu_fwd(&self, _s: &CpuStorage, _l: &Layout) -> Result<(CpuStorage, Shape)> {
        todo!("implement allreduce for cpu is not necessary for single node");
    }

    #[cfg(feature = "cuda")]
    fn cuda_fwd(
        &self,
        s: &candle::CudaStorage,
        l: &Layout,
    ) -> Result<(candle::CudaStorage, Shape)> {
        use candle::cuda_backend::WrapErr;
        let elem_count = l.shape().elem_count();
        let dev = s.device().clone();
        let s = s.as_cuda_slice::<f16>()?;
        // let s = match l.contiguous_offsets() {
        //     None => Err(Error::Wrapped("input has to be contiguous".into()))?,
        //     Some((o1, o2)) => s.slice(o1..o2),
        // };
        let mut dst = unsafe { dev.alloc::<f16>(elem_count) }.w()?;
        self.comm.all_reduce(s, &mut dst, &ReduceOp::Sum).unwrap();
        let dst = candle::CudaStorage::wrap_cuda_slice(dst, dev);
        Ok((dst, l.shape().clone()))
    }
}

fn all_reduce_sum(x: &Tensor, comm: &Rc<Comm>) -> Result<Tensor> {
    x.apply_op1(AllReduce { comm: comm.clone() })
}

impl TensorParallelRowLinear {
    fn new(linear: Linear, comm: Rc<Comm>) -> Self {
        Self { linear, comm }
    }
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let x = self.linear.forward(x)?;
        all_reduce_sum(&x, &self.comm)
    }
}

fn shard(dim: usize, rank: usize, world_size: usize) -> candle_nn::var_builder::Shard {
    candle_nn::var_builder::Shard {
        dim,
        rank,
        world_size,
    }
}

impl TensorParallelColumnLinear {
    fn load(vb: VarBuilder, comm: Rc<Comm>) -> Result<Self> {
        let rank = comm.rank();
        let size = comm.world_size();
        let weight = vb.get_with_hints((), "weight", shard(0, rank, size))?;
        Ok(Self::new(Linear::new(weight, None)))
    }

    fn load_multi(vb: VarBuilder, prefixes: &[&str], comm: Rc<Comm>) -> Result<Self> {
        let rank = comm.rank();
        let size = comm.world_size();
        let weights: Vec<_> = prefixes
            .iter()
            .map(|p| vb.pp(p).get_with_hints((), "weight", shard(0, rank, size)))
            .collect::<Result<Vec<_>>>()?;
        let weight = Tensor::cat(&weights, 0)?;
        Ok(Self::new(Linear::new(weight, None)))
    }
}

impl TensorParallelRowLinear {
    fn load(vb: VarBuilder, comm: Rc<Comm>) -> Result<Self> {
        let rank = comm.rank();
        let size = comm.world_size();
        let weight = vb.get_with_hints((), "weight", shard(1, rank, size))?;
        Ok(Self::new(Linear::new(weight, None), comm))
    }
}

#[derive(Deserialize)]
pub struct Config {
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub vocab_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub num_key_value_heads: usize,
    pub rms_norm_eps: f64,
    #[serde(default = "default_rope")]
    pub rope_theta: f32,
}

fn default_rope() -> f32 {
    10_000.0
}

#[derive(Clone)]
pub struct Cache {
    #[allow(clippy::type_complexity)]
    kvs: Arc<Mutex<Vec<Option<(Tensor, Tensor)>>>>,
    cos: Tensor,
    sin: Tensor,
}

impl Cache {
    pub fn new(dtype: DType, config: &Config, device: &Device) -> Result<Self> {
        // precompute freqs_cis
        let n_elem = config.hidden_size / config.num_attention_heads;
        let theta: Vec<_> = (0..n_elem)
            .step_by(2)
            .map(|i| 1f32 / config.rope_theta.powf(i as f32 / n_elem as f32))
            .collect();
        let theta = Tensor::new(theta.as_slice(), device)?;
        let idx_theta = Tensor::arange(0, MAX_SEQ_LEN as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((MAX_SEQ_LEN, 1))?
            .matmul(&theta.reshape((1, theta.elem_count()))?)?;
        // This is different from the paper, see:
        // https://github.com/huggingface/transformers/blob/6112b1c6442aaf7affd2b0676a1cd4eee30c45cf/src/transformers/models/llama/modeling_llama.py#L112
        let idx_theta = Tensor::cat(&[&idx_theta, &idx_theta], D::Minus1)?;
        let cos = idx_theta.cos()?.to_dtype(dtype)?;
        let sin = idx_theta.sin()?.to_dtype(dtype)?;
        Ok(Self {
            kvs: Arc::new(Mutex::new(vec![None; config.num_hidden_layers])),
            cos,
            sin,
        })
    }
}

fn silu(xs: &Tensor) -> Result<Tensor> {
    xs / (xs.neg()?.exp()? + 1.0)?
}

fn linear(size1: usize, size2: usize, vb: VarBuilder) -> Result<Linear> {
    let weight = vb.get((size2, size1), "weight")?;
    Ok(Linear::new(weight, None))
}

fn embedding(cfg: &Config, vb: VarBuilder) -> Result<Embedding> {
    let embeddings = vb.get((cfg.vocab_size, cfg.hidden_size), "weight")?;
    Ok(Embedding::new(embeddings, cfg.hidden_size))
}

struct CausalSelfAttention {
    qkv_proj: TensorParallelColumnLinear,
    o_proj: TensorParallelRowLinear,
    num_attention_heads: usize,
    num_key_value_heads: usize,
    head_dim: usize,
    cache: Cache,
}

impl CausalSelfAttention {
    fn apply_rotary_emb(&self, x: &Tensor, index_pos: usize) -> Result<Tensor> {
        let (b_sz, _, seq_len, hidden_size) = x.shape().dims4()?;
        let cos = self.cache.cos.narrow(0, index_pos, seq_len)?;
        let sin = self.cache.sin.narrow(0, index_pos, seq_len)?;
        let cos = cos.broadcast_as((b_sz, 1, seq_len, hidden_size))?;
        let sin = sin.broadcast_as((b_sz, 1, seq_len, hidden_size))?;
        let x1 = x.narrow(D::Minus1, 0, hidden_size / 2)?;
        let x2 = x.narrow(D::Minus1, hidden_size / 2, hidden_size / 2)?;
        let rotate_x = Tensor::cat(&[&x2.neg()?, &x1], D::Minus1)?;
        let rope = (x.broadcast_mul(&cos)? + rotate_x.broadcast_mul(&sin)?)?;
        Ok(rope)
    }

    fn forward(&self, x: &Tensor, index_pos: usize, block_idx: usize) -> Result<Tensor> {
        let (b_sz, seq_len, _) = x.shape().dims3()?;

        let qkv = self.qkv_proj.forward(x)?;
        let hidden_size = self.num_attention_heads * self.head_dim;

        let q = qkv.i((.., .., ..self.num_attention_heads * self.head_dim))?;
        let k = qkv.i((
            ..,
            ..,
            self.num_attention_heads * self.head_dim
                ..self.num_attention_heads * self.head_dim
                    + self.num_key_value_heads * self.head_dim,
        ))?;
        let v = qkv.i((
            ..,
            ..,
            self.num_attention_heads * self.head_dim + self.num_key_value_heads * self.head_dim..,
        ))?;
        // todo!("Q {:?} K {:?} V {:?} - x {:?}", q.shape(), k.shape(), v.shape(), x.shape());

        let q = q
            .reshape((b_sz, seq_len, self.num_attention_heads, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
            .transpose(1, 2)?;
        let mut v = v
            .reshape((b_sz, seq_len, self.num_key_value_heads, self.head_dim))?
            .transpose(1, 2)?;

        let q = self.apply_rotary_emb(&q, index_pos)?;
        let mut k = self.apply_rotary_emb(&k, index_pos)?;

        let mut cache = self.cache.kvs.lock().unwrap();
        if let Some((cache_k, cache_v)) = &cache[block_idx] {
            k = Tensor::cat(&[cache_k, &k], 2)?.contiguous()?;
            v = Tensor::cat(&[cache_v, &v], 2)?.contiguous()?;
            let k_seq_len = k.dims()[1];
            if k_seq_len > MAX_SEQ_LEN {
                k = k
                    .narrow(D::Minus1, k_seq_len - MAX_SEQ_LEN, MAX_SEQ_LEN)?
                    .contiguous()?
            }
            let v_seq_len = v.dims()[1];
            if v_seq_len > 2 * MAX_SEQ_LEN {
                v = v
                    .narrow(D::Minus1, v_seq_len - MAX_SEQ_LEN, MAX_SEQ_LEN)?
                    .contiguous()?
            }
        }
        cache[block_idx] = Some((k.clone(), v.clone()));

        let k = self.repeat_kv(k)?;
        let v = self.repeat_kv(v)?;
        let q = q.transpose(1, 2)?;
        let k = k.transpose(1, 2)?;
        let v = v.transpose(1, 2)?;
        let softmax_scale = 1f32 / (self.head_dim as f32).sqrt();
        let y = candle_flash_attn::flash_attn(&q, &k, &v, softmax_scale, seq_len > 1)?
            .transpose(1, 2)?;
        // Convert to contiguous as matmul doesn't support strided vs for now.
        let y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, hidden_size])?;
        let y = self.o_proj.forward(&y)?;
        Ok(y)
    }

    fn repeat_kv(&self, x: Tensor) -> Result<Tensor> {
        let n_rep = self.num_attention_heads / self.num_key_value_heads;
        if n_rep == 1 {
            Ok(x)
        } else {
            let (b_sz, n_kv_head, seq_len, head_dim) = x.shape().dims4()?;
            let x = x
                .unsqueeze(2)?
                .expand((b_sz, n_kv_head, n_rep, seq_len, head_dim))?
                .reshape((b_sz, n_kv_head, n_rep, seq_len, head_dim))?;
            Ok(x)
        }
    }

    fn load(vb: VarBuilder, cache: &Cache, cfg: &Config, comm: Rc<Comm>) -> Result<Self> {
        let qkv_proj = TensorParallelColumnLinear::load_multi(
            vb.clone(),
            &["q_proj", "k_proj", "v_proj"],
            comm.clone(),
        )?;
        let o_proj = TensorParallelRowLinear::load(vb.pp("o_proj"), comm.clone())?;
        Ok(Self {
            qkv_proj,
            o_proj,
            num_attention_heads: cfg.num_attention_heads / comm.world_size(),
            num_key_value_heads: cfg.num_key_value_heads / comm.world_size(),
            head_dim: cfg.hidden_size / cfg.num_attention_heads,
            cache: cache.clone(),
        })
    }

    fn clear_cache(&mut self, config: &Config) {
        self.cache.masks = Arc::new(Mutex::new(HashMap::new()));
        self.cache.kvs = Arc::new(Mutex::new(vec![None; config.num_hidden_layers]));
    }
}

struct Mlp {
    c_fc1: TensorParallelColumnLinear,
    c_fc2: TensorParallelColumnLinear,
    c_proj: TensorParallelRowLinear,
}

impl Mlp {
    fn new(
        c_fc1: TensorParallelColumnLinear,
        c_fc2: TensorParallelColumnLinear,
        c_proj: TensorParallelRowLinear,
    ) -> Self {
        Self {
            c_fc1,
            c_fc2,
            c_proj,
        }
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let x = (silu(&self.c_fc1.forward(x)?)? * self.c_fc2.forward(x)?)?;
        self.c_proj.forward(&x)
    }

    fn load(vb: VarBuilder, _cfg: &Config, comm: Rc<Comm>) -> Result<Self> {
        let c_fc1 = TensorParallelColumnLinear::load(vb.pp("gate_proj"), comm.clone())?;
        let c_fc2 = TensorParallelColumnLinear::load(vb.pp("up_proj"), comm.clone())?;
        let c_proj = TensorParallelRowLinear::load(vb.pp("down_proj"), comm)?;
        Ok(Self::new(c_fc1, c_fc2, c_proj))
    }
}

struct Block {
    rms_1: RmsNorm,
    attn: CausalSelfAttention,
    rms_2: RmsNorm,
    mlp: Mlp,
}

fn rms_norm(size: usize, eps: f64, vb: VarBuilder) -> Result<RmsNorm> {
    let weight = vb.get_with_hints(size, "weight", shard(0, 0, 1))?;
    Ok(RmsNorm::new(weight, eps))
}

impl Block {
    fn new(rms_1: RmsNorm, attn: CausalSelfAttention, rms_2: RmsNorm, mlp: Mlp) -> Self {
        Self {
            rms_1,
            attn,
            rms_2,
            mlp,
        }
    }

    fn forward(&self, x: &Tensor, index_pos: usize, block_idx: usize) -> Result<Tensor> {
        let residual = x;
        let x = self.rms_1.forward(x)?;
        let x = (self.attn.forward(&x, index_pos, block_idx)? + residual)?;
        let residual = &x;
        let x = (self.mlp.forward(&self.rms_2.forward(&x)?)? + residual)?;
        Ok(x)
    }

    fn load(vb: VarBuilder, cache: &Cache, cfg: &Config, comm: Rc<Comm>) -> Result<Self> {
        let attn = CausalSelfAttention::load(vb.pp("self_attn"), cache, cfg, comm.clone())?;
        let mlp = Mlp::load(vb.pp("mlp"), cfg, comm)?;
        let input_layernorm = rms_norm(cfg.hidden_size, 1e-5, vb.pp("input_layernorm"))?;
        let post_attention_layernorm =
            rms_norm(cfg.hidden_size, 1e-5, vb.pp("post_attention_layernorm"))?;
        Ok(Self::new(
            input_layernorm,
            attn,
            post_attention_layernorm,
            mlp,
        ))
    }
}

pub struct Llama {
    wte: Embedding,
    blocks: Vec<Block>,
    ln_f: RmsNorm,
    lm_head: Linear,
    cfg: Config,
}

impl Llama {
    fn new(
        wte: Embedding,
        blocks: Vec<Block>,
        ln_f: RmsNorm,
        lm_head: Linear,
        cfg: Config,
    ) -> Self {
        Self {
            wte,
            blocks,
            ln_f,
            lm_head,
            cfg,
        }
    }

    pub fn forward(&self, x: &Tensor, index_pos: usize) -> Result<Tensor> {
        let (_b_sz, seq_len) = x.shape().dims2()?;
        let mut x = self.wte.forward(x)?;
        for (block_idx, block) in self.blocks.iter().enumerate() {
            x = block.forward(&x, index_pos, block_idx)?;
        }
        let x = self.ln_f.forward(&x)?;
        let x = x.i((.., seq_len - 1, ..))?;
        let logits = self.lm_head.forward(&x)?;
        logits.to_dtype(DType::F32)
    }

    pub fn load(vb: VarBuilder, cache: &Cache, cfg: &Config, comm: Rc<Comm>) -> Result<Self> {
        let wte = embedding(cfg, vb.pp("model.embed_tokens"))?;
        let lm_head = linear(cfg.hidden_size, cfg.vocab_size, vb.pp("lm_head"))?;
        let norm = rms_norm(cfg.hidden_size, 1e-5, vb.pp("model.norm"))?;
        let blocks: Vec<_> = (0..cfg.num_hidden_layers)
            .map(|i| {
                Block::load(
                    vb.pp(&format!("model.layers.{i}")),
                    cache,
                    cfg,
                    comm.clone(),
                )
                .unwrap()
            })
            .collect();

        Ok(Self::new(wte, blocks, norm, lm_head, cfg.clone()))
    }

    pub fn clear_cache(&mut self, config: &Config) {
        for block in &mut self.blocks {
            block.attn.clear_cache(config);
        }
    }

    pub fn get_config(&self) -> &Config {
        &self.cfg
    }
}
