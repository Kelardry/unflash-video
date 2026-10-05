//! WebGPU pixel stage for the Unflash detector, on wgpu (native backends and
//! the browser's WebGPU alike).
//!
//! Each frame runs five compute passes, six with pattern detection on,
//! and one more before them for a YUV source (**yuv**: its planes converted
//! into the RGBA source texture). As a picture arrives:
//!
//! 1. **ingest** — area-average the source texture to the analysis size,
//!    linearise through the sRGB table and produce the L / V / chromaticity
//!    planes (the frame's input planes, kept until its batch runs).
//!
//! Then, frame by frame, when the batch runs:
//!
//! 2. **moved** — count the pixels that moved since the last new picture
//!    (for the held-frame test; it needs the previous frame's update);
//! 3. **pattern** — the regular-pattern (stripe) detector
//!    (`unflash_core::pattern`, restated in WGSL), one thread per sampling
//!    line, marking the patterned pixels;
//! 4. **update** — the per-pixel state machine
//!    (`unflash_core::pixel::run_frame_scalar`, restated in WGSL);
//! 5. **rows** — per-row window sums and onset maxima, one thread per window
//!    position (no workgroup barriers: cheap on real GPUs and not
//!    pathological on software ones);
//! 6. **gather** — one grid cell per window position.
//!
//! The output is a few kilobytes per frame, copied into a staging buffer and
//! mapped asynchronously; several frames can be in flight. Per-pixel state
//! never leaves the GPU, so the traffic per frame is one read and a partial
//! write of the ~120-byte pixel record plus the input planes: the stage runs
//! at pixel count × memory bandwidth, as it should.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bytemuck::{bytes_of, cast_slice};
use unflash_core::config::DetectorConfig;
use unflash_core::grid::{GridCell, GridGeometry, GridStats};
use unflash_core::lut::lut;
use unflash_core::pixel::{KernelParams, StateLayout, MODE_FIRST};
use unflash_core::yuv::YuvLayout;

pub use wgpu;

const PRELUDE: &str = include_str!("shaders/prelude.wgsl");
const INGEST: &str = include_str!("shaders/ingest.wgsl");
const YUV: &str = include_str!("shaders/yuv.wgsl");
const MOVED: &str = include_str!("shaders/moved.wgsl");
const PATTERN: &str = include_str!("shaders/pattern.wgsl");
const UPDATE: &str = include_str!("shaders/update.wgsl");
const ROWS: &str = include_str!("shaders/rows.wgsl");
const GATHER: &str = include_str!("shaders/gather.wgsl");

const OUT_HEADER: usize = 8;
const CELL_WORDS: usize = 12;
const GEO_WORDS: usize = 8 + 64 + 64 + 4;
const GEO_MAX_POS: usize = 64;
const GEO_PAT_R: usize = 136;
const PATTERN_WG: u32 = 64;
/// Batches that may be in flight before `submit` refuses. A detector's
/// throughput is at most slots × batch frames per submit-to-result
/// latency, and that latency can be long (in Firefox every readback crosses
/// to its GPU process: 300 ms was measured), so the page may run well
/// ahead of the results: 32 × 16 frames a round trip is 1700 frames a
/// second even there, as a scan's one detector must keep up with all its
/// decoders. A slot costs only its small readback buffers (about 50 KB).
pub const DEFAULT_SLOTS: usize = 32;
/// Frames per batch: one command buffer and one readback for this many
/// frames, so the submit-to-result latency is paid once per batch.
pub const DEFAULT_BATCH: usize = 16;
/// Dynamic buffer offsets must be multiples of this (the default limit for
/// uniform and storage bindings alike).
const REGION_ALIGN: usize = 256;

fn align_up(n: usize) -> usize {
    n.div_ceil(REGION_ALIGN) * REGION_ALIGN
}

/// An adapter + device + queue.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl GpuContext {
    /// Request a high-performance adapter and a device with default limits.
    pub async fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no WebGPU adapter: {e}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("unflash"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("device request failed: {e}"))?;
        Ok(GpuContext { instance, adapter, device, queue })
    }

    pub fn info(&self) -> wgpu::AdapterInfo {
        self.adapter.get_info()
    }
}

/// Where a frame comes from.
pub enum FrameSource<'a> {
    /// 8-bit sRGB pixels, tightly packed, any size (area-averaged down).
    Rgb8 { data: &'a [u8], width: u32, height: u32 },
    /// 8-bit sRGB pixels with an ignored fourth byte.
    Rgba8 { data: &'a [u8], width: u32, height: u32 },
    /// The same in B, G, R order (WebCodecs' BGRX / BGRA pictures).
    Bgra8 { data: &'a [u8], width: u32, height: u32 },
    /// 8-bit 4:2:0 YCbCr planes (I420 or NV12) of any size, converted to RGB
    /// on the GPU.
    Yuv420 { data: &'a [u8], width: u32, height: u32, layout: YuvLayout },
    /// The stage's own source texture, which the caller has already filled
    /// (see [`GpuStage::source_texture`]) — the browser path.
    SourceTexture,
}

enum SlotState {
    Idle,
    /// Maps still outstanding, and the first error seen.
    Pending { remaining: u32, err: Option<String> },
}

/// One batch's readback: the stats of every frame in it, and their captures.
struct Slot {
    staging: wgpu::Buffer,
    rgba_staging: Option<wgpu::Buffer>,
    /// Any frame of the batch asked for its picture.
    captured: bool,
    state: Arc<Mutex<SlotState>>,
    /// Per frame of the batch.
    params: Vec<KernelParams>,
    captures: Vec<bool>,
    /// The next frame of the batch to hand out, once mapped.
    next: usize,
    /// The mapped readbacks, copied out on the first poll of the batch.
    words: Vec<u32>,
    rgba: Vec<u8>,
    /// Results are dropped (after a reset).
    discard: bool,
}

/// Called from a readback's completion callback, once a batch's results
/// have arrived: lets a browser wake the code waiting for them instead of
/// polling on a timer (which a hidden tab slows to once a second).
#[cfg(target_arch = "wasm32")]
pub type Notify = std::rc::Rc<dyn Fn()>;
#[cfg(not(target_arch = "wasm32"))]
pub type Notify = Arc<dyn Fn() + Send + Sync>;

/// One completed frame.
#[derive(Clone, Debug)]
pub struct GpuFrame {
    pub stats: GridStats,
    /// The analysis-resolution picture as RGBA8, when capture was requested.
    pub rgba: Option<Vec<u8>>,
}

/// The GPU pixel stage. Create one per (config, analysis size).
pub struct GpuStage {
    device: wgpu::Device,
    queue: wgpu::Queue,
    cfg: DetectorConfig,
    geom: GridGeometry,
    // buffers
    params_buf: wgpu::Buffer,
    geo_buf: wgpu::Buffer,
    lut_buf: wgpu::Buffer,
    inputs_buf: wgpu::Buffer,
    /// Read back only by the native debug helpers (the passes reach it
    /// through their bind groups).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    state_buf: wgpu::Buffer,
    /// Read back only by the native debug helpers.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pixout_buf: wgpu::Buffer,
    rgba_buf: wgpu::Buffer,
    // the per-frame globals regions and the row sums are reached through
    // the bind groups only
    /// Read back only by the native debug helpers (the per-row pattern
    /// counts live in the rows / gather bind groups alone).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    patmask_buf: wgpu::Buffer,
    out_buf: wgpu::Buffer,
    out_words: usize,
    /// threads of the pattern pass: orientations × sampling lines
    pat_threads: u32,
    // source texture
    src: Option<(wgpu::Texture, wgpu::TextureView, u32, u32)>,
    rgba_scratch: Vec<u8>,
    // YUV sources: the plane textures and the conversion pass into `src`
    yuv_bgl: wgpu::BindGroupLayout,
    yuv_pipe: wgpu::ComputePipeline,
    yuv_params_buf: wgpu::Buffer,
    yuv: Option<YuvPlanes>,
    // per-frame regions inside the batch-sized buffers (bytes)
    inputs_region: usize,
    rgba_region: usize,
    out_region: usize,
    // pipelines
    ingest_bgl: wgpu::BindGroupLayout,
    ingest_pipe: wgpu::ComputePipeline,
    ingest_bg: Option<wgpu::BindGroup>,
    moved_pipe: wgpu::ComputePipeline,
    moved_bg: wgpu::BindGroup,
    pattern_pipe: wgpu::ComputePipeline,
    pattern_bg: wgpu::BindGroup,
    update_pipe: wgpu::ComputePipeline,
    update_bg: wgpu::BindGroup,
    rows_pipe: wgpu::ComputePipeline,
    rows_bg: wgpu::BindGroup,
    gather_pipe: wgpu::ComputePipeline,
    gather_bg: wgpu::BindGroup,
    // the batch being filled
    batch: usize,
    current: Option<usize>,
    queued: usize,
    frame_params: Vec<KernelParams>,
    frame_capture: Vec<bool>,
    /// Region of the last frame ingested (for the native debug readbacks).
    last_pos: usize,
    // readback ring
    slots: Vec<Slot>,
    in_flight: VecDeque<usize>,
    /// Told whenever a readback completes.
    notify: Option<Notify>,
    free: Vec<usize>,
}

fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// A storage binding addressed per frame: the bind group binds one region and
/// the dispatch supplies the frame's offset.
fn dyn_storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Storage { read_only }, has_dynamic_offset: true, min_binding_size: None }, ..storage_entry(binding, read_only) }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: true,
            min_binding_size: None,
        },
        count: None,
    }
}

/// The first `size` bytes of `buf`, moved along by a dynamic offset per frame.
fn region_entry(binding: u32, buf: &wgpu::Buffer, size: usize) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: buf, offset: 0, size: wgpu::BufferSize::new(size as u64) }) }
}

/// The plane textures of a YUV source at one size, and the bind group of
/// the conversion pass writing them into the source texture.
struct YuvPlanes {
    y: wgpu::Texture,
    /// Cb (I420) or interleaved CbCr (NV12)
    c: wgpu::Texture,
    /// Cr (I420 only)
    c2: Option<wgpu::Texture>,
    width: u32,
    height: u32,
    nv12: bool,
    bg: wgpu::BindGroup,
}

fn storage_texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format: wgpu::TextureFormat::Rgba8Unorm, view_dimension: wgpu::TextureViewDimension::D2 },
        count: None,
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn buf_entry(binding: u32, buf: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry { binding, resource: buf.as_entire_binding() }
}

impl GpuStage {
    pub fn new(ctx: &GpuContext, cfg: &DetectorConfig, geom: GridGeometry) -> Result<Self, String> {
        Self::with_options(ctx, cfg, geom, DEFAULT_SLOTS, DEFAULT_BATCH)
    }

    /// `nslots` batches may be in flight; `batch` frames go into one command
    /// buffer and one readback (1 for a result after every frame, as the live
    /// monitor wants).
    pub fn with_options(ctx: &GpuContext, cfg: &DetectorConfig, geom: GridGeometry, nslots: usize, batch: usize) -> Result<Self, String> {
        let batch = batch.max(1);
        let device = ctx.device.clone();
        let queue = ctx.queue.clone();
        // (the analysis model's screen at full scale: its per-pixel state,
        // about 100 MB at 1024×768, is near what one storage binding may
        // hold by default, 128 MiB)
        if geom.aw > 1024 {
            return Err(format!("analysis width {} is over 1024, the analysis model's at full scale", geom.aw));
        }
        if geom.gxs.len() > GEO_MAX_POS || geom.gys.len() > GEO_MAX_POS {
            return Err("too many window positions".into());
        }
        let k = cfg.k_fail() as usize;
        let layout = StateLayout { k };
        let npix = geom.npix();
        let ncells = geom.ncells();
        let out_words = OUT_HEADER + ncells * CELL_WORDS;

        let mk = |label: &str, size: usize, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (size.max(4)) as u64,
                usage,
                mapped_at_creation: false,
            })
        };
        let st = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC;
        // the per-frame buffers hold one region per frame of a batch
        let inputs_region = align_up(3 * npix * 4);
        let rgba_region = align_up(npix * 4);
        let out_region = align_up(out_words * 4);
        let params_buf = mk("params", batch * REGION_ALIGN, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
        let geo_buf = mk("geo", GEO_WORDS * 4, st);
        let lut_buf = mk("lut", 256 * 4, st);
        let inputs_buf = mk("inputs", batch * inputs_region, st);
        let state_buf = mk("state", layout.fields() * npix * 4, st);
        let pixout_buf = mk("pixout", 3 * npix * 4, st);
        let rgba_buf = mk("rgba", batch * rgba_region, st);
        let globals_buf = mk("globals", batch * REGION_ALIGN, st);
        let rowwin_buf = mk("rowwin", geom.ah as usize * geom.gxs.len() * CELL_WORDS * 4, st);
        let rowtot_buf = mk("rowtot", geom.ah as usize * 4, st);
        let patmask_buf = mk("patmask", npix * 4, st);
        let rowpat_buf = mk("rowpat", geom.ah as usize * 4, st);
        let out_buf = mk("out", batch * out_region, st);
        let pat_r = unflash_core::pattern::line_radius(geom.aw, geom.ah);
        let pat_threads = unflash_core::pattern::ORIENTATIONS as u32 * (2 * pat_r as u32 + 1);

        // static uploads
        let mut geo_words = vec![0u32; GEO_WORDS];
        geo_words[2] = geom.aw;
        geo_words[3] = geom.ah;
        geo_words[4] = geom.ww;
        geo_words[5] = geom.wh;
        geo_words[6] = geom.gxs.len() as u32;
        geo_words[7] = geom.gys.len() as u32;
        for (i, &g) in geom.gxs.iter().enumerate() {
            geo_words[8 + i] = g;
        }
        for (i, &g) in geom.gys.iter().enumerate() {
            geo_words[8 + 64 + i] = g;
        }
        geo_words[GEO_PAT_R] = pat_r as u32;
        queue.write_buffer(&geo_buf, 0, cast_slice(&geo_words));
        queue.write_buffer(&lut_buf, 0, cast_slice(lut()));
        queue.write_buffer(&globals_buf, 0, &vec![0u8; batch * REGION_ALIGN]);

        // shaders
        let assemble = |src: &str| -> String {
            let mut s = String::from(PRELUDE);
            s.push_str(src);
            s.replace("{{K}}", &format!("{k}u"))
        };
        let module = |label: &str, src: String| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(Cow::Owned(src)) })
        };
        let ingest_mod = module("ingest", assemble(INGEST));
        let moved_mod = module("moved", assemble(MOVED));
        let yuv_mod = module("yuv", assemble(YUV));
        let pattern_mod = module("pattern", assemble(PATTERN));
        let update_mod = module("update", assemble(UPDATE));
        let rows_mod = module("rows", assemble(ROWS));
        let gather_mod = module("gather", assemble(GATHER));

        let bgl = |label: &str, entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries })
        };
        // params, inputs, globals, rgba and out are addressed per frame
        let ingest_bgl = bgl("ingest", &[uniform_entry(0), storage_entry(1, true), storage_entry(2, true), texture_entry(3), dyn_storage_entry(4, false), dyn_storage_entry(5, false)]);
        let yuv_bgl = bgl(
            "yuv",
            &[
                wgpu::BindGroupLayoutEntry { ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, ..uniform_entry(0) },
                texture_entry(1),
                texture_entry(2),
                texture_entry(3),
                storage_texture_entry(4),
            ],
        );
        let moved_bgl = bgl("moved", &[uniform_entry(0), dyn_storage_entry(1, true), storage_entry(2, true), dyn_storage_entry(3, false)]);
        let pattern_bgl = bgl("pattern", &[uniform_entry(0), storage_entry(1, true), dyn_storage_entry(2, true), storage_entry(3, false), dyn_storage_entry(4, false)]);
        let update_bgl = bgl("update", &[uniform_entry(0), dyn_storage_entry(1, true), storage_entry(2, false), storage_entry(3, false), dyn_storage_entry(4, true)]);
        let rows_bgl = bgl(
            "rows",
            &[uniform_entry(0), storage_entry(1, true), dyn_storage_entry(2, true), storage_entry(3, true), storage_entry(4, false), storage_entry(5, false), storage_entry(6, true), storage_entry(7, false)],
        );
        let gather_bgl = bgl(
            "gather",
            &[uniform_entry(0), storage_entry(1, true), storage_entry(2, true), storage_entry(3, true), dyn_storage_entry(4, false), dyn_storage_entry(5, false), storage_entry(6, true)],
        );

        let pipe = |label: &str, l: &wgpu::BindGroupLayout, m: &wgpu::ShaderModule| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(l)], immediate_size: 0 });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pl),
                module: m,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let ingest_pipe = pipe("ingest", &ingest_bgl, &ingest_mod);
        let yuv_pipe = pipe("yuv", &yuv_bgl, &yuv_mod);
        let yuv_params_buf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("yuv params"), size: 48, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let moved_pipe = pipe("moved", &moved_bgl, &moved_mod);
        let pattern_pipe = pipe("pattern", &pattern_bgl, &pattern_mod);
        let update_pipe = pipe("update", &update_bgl, &update_mod);
        let rows_pipe = pipe("rows", &rows_bgl, &rows_mod);
        let gather_pipe = pipe("gather", &gather_bgl, &gather_mod);

        let params_size = std::mem::size_of::<KernelParams>();
        let moved_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("moved"),
            layout: &moved_bgl,
            entries: &[region_entry(0, &params_buf, params_size), region_entry(1, &inputs_buf, inputs_region), buf_entry(2, &state_buf), region_entry(3, &globals_buf, REGION_ALIGN)],
        });
        let pattern_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pattern"),
            layout: &pattern_bgl,
            entries: &[region_entry(0, &params_buf, params_size), buf_entry(1, &geo_buf), region_entry(2, &inputs_buf, inputs_region), buf_entry(3, &patmask_buf), region_entry(4, &globals_buf, REGION_ALIGN)],
        });
        let update_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("update"),
            layout: &update_bgl,
            entries: &[region_entry(0, &params_buf, params_size), region_entry(1, &inputs_buf, inputs_region), buf_entry(2, &state_buf), buf_entry(3, &pixout_buf), region_entry(4, &globals_buf, REGION_ALIGN)],
        });
        let rows_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rows"),
            layout: &rows_bgl,
            entries: &[
                region_entry(0, &params_buf, params_size),
                buf_entry(1, &geo_buf),
                region_entry(2, &inputs_buf, inputs_region),
                buf_entry(3, &pixout_buf),
                buf_entry(4, &rowwin_buf),
                buf_entry(5, &rowtot_buf),
                buf_entry(6, &patmask_buf),
                buf_entry(7, &rowpat_buf),
            ],
        });
        let gather_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gather"),
            layout: &gather_bgl,
            entries: &[
                region_entry(0, &params_buf, params_size),
                buf_entry(1, &geo_buf),
                buf_entry(2, &rowwin_buf),
                buf_entry(3, &rowtot_buf),
                region_entry(4, &globals_buf, REGION_ALIGN),
                region_entry(5, &out_buf, out_region),
                buf_entry(6, &rowpat_buf),
            ],
        });

        let slots = (0..nslots.max(1))
            .map(|i| Slot {
                staging: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&format!("staging{i}")),
                    size: (batch * out_region) as u64,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                rgba_staging: None,
                captured: false,
                state: Arc::new(Mutex::new(SlotState::Idle)),
                params: Vec::new(),
                captures: Vec::new(),
                next: 0,
                words: Vec::new(),
                rgba: Vec::new(),
                discard: false,
            })
            .collect::<Vec<_>>();
        let free = (0..slots.len()).rev().collect();

        Ok(GpuStage {
            device,
            queue,
            cfg: cfg.clone(),
            geom,
            params_buf,
            geo_buf,
            lut_buf,
            inputs_buf,
            state_buf,
            pixout_buf,
            rgba_buf,
            patmask_buf,
            out_buf,
            out_words,
            pat_threads,
            src: None,
            rgba_scratch: Vec::new(),
            inputs_region,
            rgba_region,
            out_region,
            yuv_bgl,
            yuv_pipe,
            yuv_params_buf,
            yuv: None,
            ingest_bgl,
            ingest_pipe,
            ingest_bg: None,
            moved_pipe,
            moved_bg,
            pattern_pipe,
            pattern_bg,
            update_pipe,
            update_bg,
            rows_pipe,
            rows_bg,
            gather_pipe,
            gather_bg,
            batch,
            current: None,
            queued: 0,
            frame_params: Vec::with_capacity(batch),
            frame_capture: Vec::with_capacity(batch),
            last_pos: 0,
            slots,
            in_flight: VecDeque::new(),
            notify: None,
            free,
        })
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }
    /// Have `f` called whenever a batch's readback completes (see [`Notify`]).
    pub fn set_notify(&mut self, f: Option<Notify>) {
        self.notify = f;
    }
    /// Whether `poll` has something to hand out now, or nothing is in flight
    /// to wait for.
    pub fn ready(&self) -> bool {
        match self.in_flight.front() {
            None => true,
            Some(&i) => {
                let slot = &self.slots[i];
                slot.next > 0 || !slot.words.is_empty() || matches!(*slot.state.lock().unwrap(), SlotState::Pending { remaining: 0, .. })
            }
        }
    }
    /// Room for another frame: the batch being filled has some, or a batch
    /// slot is free to start one.
    pub fn can_submit(&self) -> bool {
        self.current.is_some() || !self.free.is_empty()
    }
    /// Bytes of per-pixel state touched by one full-update frame: the record
    /// read plus the partial write plus the input planes (for bandwidth
    /// accounting in benchmarks).
    pub fn bytes_per_frame(&self) -> u64 {
        let n = self.geom.npix() as u64;
        // inputs (l, v, sat) written by ingest and read by update + rows;
        // state fields read (≈20) and written (≈8); pixout written and read;
        // the pattern pass reads L once per orientation and clears / marks /
        // counts the mask
        let pattern = if self.cfg.flag_patterns() { unflash_core::pattern::ORIENTATIONS as u64 + 3 } else { 0 };
        n * 4 * (3 * 3 + 20 + 8 + 3 * 2 + pattern)
    }

    /// The source texture at the given size, (re)created as needed. Fill it
    /// (e.g. with `queue.copy_external_image_to_texture` in a browser) and
    /// then submit with [`FrameSource::SourceTexture`].
    pub fn source_texture(&mut self, width: u32, height: u32) -> &wgpu::Texture {
        let needs = match &self.src {
            Some((_, _, w, h)) => *w != width || *h != height,
            None => true,
        };
        if needs {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("source"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::STORAGE_BINDING,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            // the conversion pass writes into this texture: rebind it
            self.yuv = None;
            self.ingest_bg = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ingest"),
                layout: &self.ingest_bgl,
                entries: &[
                    region_entry(0, &self.params_buf, std::mem::size_of::<KernelParams>()),
                    buf_entry(1, &self.geo_buf),
                    buf_entry(2, &self.lut_buf),
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&view) },
                    region_entry(4, &self.inputs_buf, self.inputs_region),
                    region_entry(5, &self.rgba_buf, self.rgba_region),
                ],
            }));
            let mut geo = [0u32; 2];
            geo[0] = width;
            geo[1] = height;
            self.queue.write_buffer(&self.geo_buf, 0, cast_slice(&geo));
            self.src = Some((tex, view, width, height));
        }
        &self.src.as_ref().unwrap().0
    }

    /// Submit one frame: its picture is converted now, and the detector runs
    /// over it when its batch is full (or on [`flush`](Self::flush)). Fails
    /// (without side effects) when every batch slot is in flight; call
    /// [`poll`](Self::poll) first. With `capture`, the analysis-resolution
    /// RGBA picture comes back with the result.
    pub fn submit(&mut self, mut params: KernelParams, source: FrameSource<'_>, capture: bool) -> Result<(), String> {
        let started_here = self.current.is_none();
        let slot_idx = match self.current {
            Some(s) => s,
            None => match self.free.pop() {
                Some(s) => s,
                None => return Err("all readback slots are in flight".into()),
            },
        };
        let k = self.queued;
        let mut yuv_pass = false;
        // 1. source
        match source {
            FrameSource::Rgb8 { data, width, height } => {
                let n = (width * height) as usize;
                if data.len() < n * 3 {
                    if started_here {
                        self.free.push(slot_idx);
                    }
                    return Err("frame data too short".into());
                }
                self.rgba_scratch.resize(n * 4, 255);
                for i in 0..n {
                    self.rgba_scratch[i * 4..i * 4 + 3].copy_from_slice(&data[i * 3..i * 3 + 3]);
                }
                let tex = self.source_texture(width, height).clone();
                self.write_source(&tex, &self.rgba_scratch, width, height);
            }
            FrameSource::Rgba8 { data, width, height } | FrameSource::Bgra8 { data, width, height } => {
                let n = (width * height) as usize;
                if data.len() < n * 4 {
                    if started_here {
                        self.free.push(slot_idx);
                    }
                    return Err("frame data too short".into());
                }
                let tex = self.source_texture(width, height).clone();
                self.write_source(&tex, data, width, height);
                // the ingest pass swaps the channels back
                params.src_bgr = matches!(source, FrameSource::Bgra8 { .. }) as u32;
            }
            FrameSource::SourceTexture => {
                if self.src.is_none() {
                    if started_here {
                        self.free.push(slot_idx);
                    }
                    return Err("no source texture has been created".into());
                }
            }
            FrameSource::Yuv420 { data, width, height, layout } => {
                if width == 0 || height == 0 || !layout.fits(data.len(), width as usize, height as usize) {
                    if started_here {
                        self.free.push(slot_idx);
                    }
                    return Err("picture data too short for its layout".into());
                }
                self.source_texture(width, height);
                self.yuv_planes(width, height, layout.nv12);
                let k = layout.coefficients();
                let mut p = [0u32; 12];
                p[0] = width;
                p[1] = height;
                p[2] = layout.nv12 as u32;
                p[3] = layout.full_range as u32;
                for i in 0..5 {
                    p[4 + i] = (k[i] as f32 / 65536.0).to_bits();
                }
                p[9] = (k[5] as f32).to_bits();
                self.queue.write_buffer(&self.yuv_params_buf, 0, cast_slice(&p));
                let planes = self.yuv.as_ref().unwrap();
                let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
                self.write_plane(&planes.y, &data[layout.y_off..], layout.y_stride as u32, width, height, 1);
                if layout.nv12 {
                    self.write_plane(&planes.c, &data[layout.u_off..], layout.u_stride as u32, cw, ch, 2);
                } else {
                    self.write_plane(&planes.c, &data[layout.u_off..], layout.u_stride as u32, cw, ch, 1);
                    self.write_plane(planes.c2.as_ref().unwrap(), &data[layout.v_off..], layout.v_stride as u32, cw, ch, 1);
                }
                yuv_pass = true;
            }
        }
        // 2. this frame's params, in its region
        self.queue.write_buffer(&self.params_buf, (k * REGION_ALIGN) as u64, bytes_of(&params));
        // 3. the picture into this frame's input planes, now: the source
        // texture is reused by the next frame
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("unflash ingest") });
        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("ingest"), timestamp_writes: None });
            if yuv_pass {
                let p = self.yuv.as_ref().unwrap();
                pass.set_pipeline(&self.yuv_pipe);
                pass.set_bind_group(0, &p.bg, &[]);
                pass.dispatch_workgroups(p.width.div_ceil(16), p.height.div_ceil(16), 1);
            }
            pass.set_pipeline(&self.ingest_pipe);
            pass.set_bind_group(0, self.ingest_bg.as_ref().unwrap(), &[(k * REGION_ALIGN) as u32, (k * self.inputs_region) as u32, (k * self.rgba_region) as u32]);
            pass.dispatch_workgroups(self.geom.aw.div_ceil(16), self.geom.ah.div_ceil(16), 1);
        }
        self.queue.submit(Some(enc.finish()));
        self.current = Some(slot_idx);
        self.frame_params.push(params);
        self.frame_capture.push(capture);
        self.queued += 1;
        self.last_pos = k;
        if self.queued >= self.batch {
            self.submit_batch();
        }
        Ok(())
    }

    /// Run the detector over the frames ingested so far and start their
    /// readback; a no-op with nothing queued. Results follow through
    /// [`poll`](Self::poll).
    pub fn flush(&mut self) {
        if self.current.is_some() && self.queued > 0 {
            self.submit_batch();
        }
    }

    /// Forget the frames ingested into the batch being filled, and drop the
    /// results of the batches in flight when they arrive.
    pub fn abandon(&mut self) {
        if let Some(s) = self.current.take() {
            self.free.push(s);
        }
        self.queued = 0;
        self.frame_params.clear();
        self.frame_capture.clear();
        for &s in &self.in_flight {
            self.slots[s].discard = true;
        }
    }

    /// The batch being filled: moved count, pattern, update, rows and gather
    /// for each frame in order, one command buffer, one readback.
    fn submit_batch(&mut self) {
        let Some(slot_idx) = self.current.take() else { return };
        let n = self.queued;
        let npix = self.geom.npix();
        let pattern = self.frame_params.iter().any(|p| p.pat_enabled != 0);
        let capture = self.frame_capture.iter().any(|&c| c);
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("unflash batch") });
        for k in 0..n {
            let p = self.frame_params[k];
            let params_off = (k * REGION_ALIGN) as u32;
            let inputs_off = (k * self.inputs_region) as u32;
            let globals_off = (k * REGION_ALIGN) as u32;
            let out_off = (k * self.out_region) as u32;
            if pattern {
                // the pattern pass ORs into the mask
                enc.clear_buffer(&self.patmask_buf, 0, None);
            }
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("detector"), timestamp_writes: None });
            pass.set_pipeline(&self.moved_pipe);
            pass.set_bind_group(0, &self.moved_bg, &[params_off, inputs_off, globals_off]);
            pass.dispatch_workgroups((npix as u32).div_ceil(256), 1, 1);
            if p.pat_enabled != 0 {
                pass.set_pipeline(&self.pattern_pipe);
                pass.set_bind_group(0, &self.pattern_bg, &[params_off, inputs_off, globals_off]);
                pass.dispatch_workgroups(self.pat_threads.div_ceil(PATTERN_WG), 1, 1);
            }
            pass.set_pipeline(&self.update_pipe);
            pass.set_bind_group(0, &self.update_bg, &[params_off, inputs_off, globals_off]);
            pass.dispatch_workgroups((npix as u32).div_ceil(256), 1, 1);
            pass.set_pipeline(&self.rows_pipe);
            pass.set_bind_group(0, &self.rows_bg, &[params_off, inputs_off]);
            pass.dispatch_workgroups(self.geom.ah, 1, 1);
            pass.set_pipeline(&self.gather_pipe);
            pass.set_bind_group(0, &self.gather_bg, &[params_off, globals_off, out_off]);
            pass.dispatch_workgroups((self.geom.ncells() as u32).div_ceil(64), 1, 1);
        }
        let slot = &mut self.slots[slot_idx];
        enc.copy_buffer_to_buffer(&self.out_buf, 0, &slot.staging, 0, (n * self.out_region) as u64);
        if capture {
            let size = (self.batch * self.rgba_region) as u64;
            let rs = slot.rgba_staging.get_or_insert_with(|| {
                self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("rgba staging"), size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false })
            });
            enc.copy_buffer_to_buffer(&self.rgba_buf, 0, rs, 0, (n * self.rgba_region) as u64);
        }
        self.queue.submit(Some(enc.finish()));
        // the async readback
        slot.params = std::mem::take(&mut self.frame_params);
        slot.captures = std::mem::take(&mut self.frame_capture);
        slot.captured = capture;
        slot.next = 0;
        slot.words.clear();
        slot.rgba.clear();
        slot.discard = false;
        *slot.state.lock().unwrap() = SlotState::Pending { remaining: if capture { 2 } else { 1 }, err: None };
        let done = |st: &Arc<Mutex<SlotState>>, r: Result<(), wgpu::BufferAsyncError>| {
            let mut g = st.lock().unwrap();
            if let SlotState::Pending { remaining, err } = &mut *g {
                *remaining = remaining.saturating_sub(1);
                if let Err(e) = r {
                    err.get_or_insert(e.to_string());
                }
            }
        };
        let st = slot.state.clone();
        let nt = self.notify.clone();
        slot.staging.slice(..(n * self.out_region) as u64).map_async(wgpu::MapMode::Read, move |r| {
            done(&st, r);
            if let Some(f) = &nt {
                f();
            }
        });
        if capture {
            let st = slot.state.clone();
            let nt = self.notify.clone();
            slot.rgba_staging.as_ref().unwrap().slice(..(n * self.rgba_region) as u64).map_async(wgpu::MapMode::Read, move |r| {
                done(&st, r);
                if let Some(f) = &nt {
                    f();
                }
            });
        }
        self.in_flight.push_back(slot_idx);
        self.queued = 0;
    }

    /// The plane textures for a YUV source of this size and kind, (re)created
    /// as needed together with the conversion pass's bind group (which also
    /// holds the source texture, so call `source_texture` first).
    fn yuv_planes(&mut self, width: u32, height: u32, nv12: bool) {
        let needs = match &self.yuv {
            Some(p) => p.width != width || p.height != height || p.nv12 != nv12,
            None => true,
        };
        if !needs {
            return;
        }
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let mk = |label: &str, w: u32, h: u32, format: wgpu::TextureFormat| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = mk("yuv y", width, height, wgpu::TextureFormat::R8Unorm);
        let c = mk("yuv c", cw, ch, if nv12 { wgpu::TextureFormat::Rg8Unorm } else { wgpu::TextureFormat::R8Unorm });
        let c2 = (!nv12).then(|| mk("yuv v", cw, ch, wgpu::TextureFormat::R8Unorm));
        let yv = y.create_view(&wgpu::TextureViewDescriptor::default());
        let cv = c.create_view(&wgpu::TextureViewDescriptor::default());
        let c2v = c2.as_ref().map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
        let dst = &self.src.as_ref().expect("source texture before the planes").1;
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.yuv_bgl,
            entries: &[
                buf_entry(0, &self.yuv_params_buf),
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&yv) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&cv) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(c2v.as_ref().unwrap_or(&cv)) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(dst) },
            ],
        });
        self.yuv = Some(YuvPlanes { y, c, c2, width, height, nv12, bg });
    }

    /// Upload one plane; rows are `stride` bytes apart (the last row may be
    /// shorter than the stride).
    fn write_plane(&self, tex: &wgpu::Texture, data: &[u8], stride: u32, width: u32, height: u32, bpp: u32) {
        let needed = ((height - 1) * stride + width * bpp) as usize;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &data[..needed],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
    }

    fn write_source(&self, tex: &wgpu::Texture, rgba: &[u8], width: u32, height: u32) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            rgba,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width * 4), rows_per_image: None },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
    }

    /// The oldest in-flight frame's result, if it has arrived. Call
    /// repeatedly (each browser animation frame, or after `device.poll` on
    /// native).
    pub fn poll(&mut self) -> Option<Result<GpuFrame, String>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = self.device.poll(wgpu::PollType::Poll);
        }
        loop {
            let &slot_idx = self.in_flight.front()?;
            let ready: Option<Result<(), String>> = {
                let st = self.slots[slot_idx].state.lock().unwrap();
                match &*st {
                    SlotState::Pending { remaining: 0, err } => Some(match err {
                        Some(e) => Err(e.clone()),
                        None => Ok(()),
                    }),
                    _ => None,
                }
            };
            let r = ready?;
            let n = self.slots[slot_idx].params.len();
            let out_region = self.out_region;
            let rgba_region = self.rgba_region;
            let npix = self.geom.npix();
            // the first look at a finished batch copies its readbacks out and
            // unmaps, so the slot can be reused while its frames are handed out
            if self.slots[slot_idx].next == 0 && self.slots[slot_idx].words.is_empty() {
                let slot = &mut self.slots[slot_idx];
                if r.is_ok() {
                    let view = slot.staging.slice(..(n * out_region) as u64).get_mapped_range().expect("mapped range");
                    slot.words = cast_slice::<u8, u32>(&view).to_vec();
                    drop(view);
                    if slot.captured {
                        let rs = slot.rgba_staging.as_ref().unwrap();
                        slot.rgba = rs.slice(..(n * rgba_region) as u64).get_mapped_range().expect("mapped rgba").to_vec();
                    }
                }
                slot.staging.unmap();
                if slot.captured {
                    if let Some(rs) = &slot.rgba_staging {
                        rs.unmap();
                    }
                }
            }
            let discard = self.slots[slot_idx].discard;
            let k = self.slots[slot_idx].next;
            let last = k + 1 >= n;
            let out = match &r {
                Ok(()) if !discard => {
                    let slot = &self.slots[slot_idx];
                    let words = &slot.words[k * out_region / 4..(k + 1) * out_region / 4];
                    let stats = self.parse(words, slot.params[k]);
                    let rgba = if slot.captures[k] { Some(slot.rgba[k * rgba_region..k * rgba_region + npix * 4].to_vec()) } else { None };
                    Some(Ok(GpuFrame { stats, rgba }))
                }
                Ok(()) => None,
                Err(e) if !discard => Some(Err(e.clone())),
                Err(_) => None,
            };
            if last {
                let slot = &mut self.slots[slot_idx];
                slot.words = Vec::new();
                slot.rgba = Vec::new();
                slot.params.clear();
                slot.captures.clear();
                slot.next = 0;
                slot.discard = false;
                *slot.state.lock().unwrap() = SlotState::Idle;
                self.in_flight.pop_front();
                self.free.push(slot_idx);
            } else {
                self.slots[slot_idx].next = k + 1;
            }
            if let Some(o) = out {
                return Some(o);
            }
            // a discarded batch: on to the next
        }
    }

    fn parse(&self, words: &[u32], params: KernelParams) -> GridStats {
        debug_assert!(words.len() >= self.out_words);
        let sum_l = f32::from_bits(words[0]) as f64;
        let held_count = words[1];
        let first = params.mode & MODE_FIRST != 0;
        let held = !first && held_count < params.held_bar;
        let cells = if held {
            Vec::new()
        } else {
            cast_slice::<u32, GridCell>(&words[OUT_HEADER..OUT_HEADER + self.geom.ncells() * CELL_WORDS]).to_vec()
        };
        GridStats { held, held_count, sum_l, cells, pattern_count: words[4], pattern_spacing_sum: words[5], pattern_spacing_n: words[6] }
    }

    /// Block until every in-flight frame is done (native only; a no-op on
    /// the web, where results arrive through the event loop).
    pub fn wait_idle(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        }
    }

    /// Native debug readback of a whole buffer.
    #[cfg(not(target_arch = "wasm32"))]
    fn read_buffer_words(&self, buf: &wgpu::Buffer) -> Vec<u32> {
        let size = buf.size();
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("debug staging"),
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_buffer_to_buffer(buf, 0, &staging, 0, size);
        self.queue.submit(Some(enc.finish()));
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        rx.recv().unwrap().expect("map failed");
        let words = cast_slice::<u8, u32>(&staging.slice(..).get_mapped_range().expect("mapped range")).to_vec();
        staging.unmap();
        words
    }

    /// The whole per-pixel state in `StateLayout` order (native, for tests).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn debug_state(&self) -> Vec<u32> {
        self.read_buffer_words(&self.state_buf)
    }

    /// The L / V / sat planes of the last frame ingested (native, for tests).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn debug_inputs(&self) -> (Vec<f32>, Vec<f32>, Vec<u32>) {
        let all = self.read_buffer_words(&self.inputs_buf);
        let w = &all[self.last_pos * self.inputs_region / 4..];
        let n = self.geom.npix();
        (
            w[..n].iter().map(|&b| f32::from_bits(b)).collect(),
            w[n..2 * n].iter().map(|&b| f32::from_bits(b)).collect(),
            w[2 * n..3 * n].to_vec(),
        )
    }

    /// The mask / onset planes of the last frame (native, for tests).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn debug_pixout(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let w = self.read_buffer_words(&self.pixout_buf);
        let n = self.geom.npix();
        (w[..n].to_vec(), w[n..2 * n].to_vec(), w[2 * n..3 * n].to_vec())
    }

    /// The pattern mask of the last frame, bit k = orientation k (native,
    /// for tests).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn debug_patmask(&self) -> Vec<u32> {
        self.read_buffer_words(&self.patmask_buf)
    }
}
