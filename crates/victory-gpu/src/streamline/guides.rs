//! The guide buffers DLSS Ray Reconstruction takes, made from the title's
//! G-buffer on the card. Off unless `VICTORY_DLSS_RR` is set and the
//! feature loaded (`super::RR_READY`).
//!
//! Why this is needed. Ray Reconstruction wants, besides colour, depth and
//! motion vectors, shading normals as float RGB (view or world space),
//! linear roughness (packed into the normals' w here), diffuse albedo and
//! specular albedo, all linear. The title keeps 8-bit encoded targets and no
//! specular albedo at all. So a small compute pass turns the title's targets
//! into three textures of ours in the formats the guide names, and derives
//! the specular albedo the way the guide's `EnvBRDFApprox2` does, from the
//! reflectance at normal incidence, the roughness and N.V.
//!
//! Finding the targets. Nothing here names a pass or a shader. The title's
//! deferred material draws are the ones that also write the motion vectors
//! DLSS is given (`MOTION_TARGET`, noted at each upscaling). The first four
//! other render targets of such a draw, in bound order, are the G-buffer.
//! That order and these formats are what run 17's target log showed (the
//! first target typeless or sRGB 8-bit, the next three 8-bit unorm, all the
//! render size); a set that does not look like that is not taken. What each
//! target means (which holds the normals, where roughness is) is a reading
//! of formats and order only until the pictures of a run are looked at. The
//! reading is set by three switches, read once in [`reading`]:
//! `VICTORY_RR_NORMALS` (`view-xyz`, `octahedral` or `world-xyz`),
//! `VICTORY_RR_GBUFFER` (`<albedo>,<normals>,<roughness>,<metalness>`, the
//! default `0,1,2.r,2.g`; roughness may be `gloss:<i>.<c>`, metalness may be
//! `none`) and `VICTORY_RR_ALBEDO` (`srgb` or `linear`). The defaults are the
//! reading of formats and bind order and are not confirmed; the line logged
//! by [`prepare`] says what is in use. World-space normals are refused,
//! because the layer has no camera view matrix to give Ray Reconstruction.
//!
//! Where the bytes are. The title's targets are read as the host copies
//! they already are, on the card, as is the depth DLSS is given. Only the
//! targets the reading names are loaded (three with the default, 12 bytes a
//! pixel) plus the depth. Our three
//! textures are in default heaps on the card (8 bytes a pixel for normals
//! and roughness, 4 each for the two albedos). Nothing crosses the bus, the
//! CPU never touches a pixel and nothing is read back. At 1706x960 a frame
//! reads about 25 MB and writes about 26 MB; the cost has not been measured.
//! Descriptors are written by the CPU into a shader-visible ring (8 a set, 256
//! sets, a set only when the views change), the only CPU work besides one dispatch.
//!
//! Threads. [`pass`] runs on any recording thread, once a draw, and returns
//! after one atomic load with the switch unset. [`upscaling`] and [`make`]
//! run on the title's thread at the upscale, [`make`] inside the work that
//! records the upscaling into the title's list. [`prepare`] starts a worker
//! that compiles the pass, and the textures are made by a worker too, so
//! neither the compile nor a large allocation is ever done on a thread the
//! title waits on; [`make`] says why it has nothing until they are there and
//! super resolution takes those upscalings.
//!
//! The limits: the draws are seen as they are recorded, ahead of the card
//! and on several threads, but the textures are the same objects every
//! frame, so recording order against card order does not matter. A frame in
//! which no draw wrote the G-buffer (a menu, a load) has no guides.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, s};

use super::{MOTION, SHADER_READ, UNORDERED, Upscale, plan};
use crate::host::{HostList, host};
use crate::object::This;

/// Resource state of a texture a later pass reads in a compute shader.
const NON_PIXEL_READ: u32 = 0x40;
/// Sets of descriptors the ring holds before one is written again. A set is
/// written only when the views change (the title's textures are the same
/// objects every frame), so this many changes pass before a slot that a list
/// in flight may read is rewritten; the host's own ring is as deep.
const FRAMES: u32 = 256;
/// Descriptors a frame: four G-buffer targets and the depth, then our three.
const SLOTS: u32 = 8;

/// The title's G-buffer: the first four render targets, in bound order, of a
/// draw that also writes the upscaling's motion target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GBuffer {
    /// The title's resources (their addresses).
    pub targets: [usize; 4],
    /// Their size, the motion target's.
    pub size: [u32; 2],
    /// Their DXGI formats.
    pub formats: [i32; 4],
}

/// How the normals are stored in the title's normals target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Normals {
    /// rgb holds xyz * 0.5 + 0.5, view space, +z toward the eye, +y up.
    ViewXyz,
    /// rg holds an octahedral unit vector * 0.5 + 0.5, in the same view space.
    Octahedral,
    /// rgb holds xyz * 0.5 + 0.5 in world space; refused (no view matrix).
    WorldXyz,
}

/// One channel of one G-buffer target (`channel` 0 to 3 is r, g, b, a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Channel {
    target: usize,
    channel: usize,
}

/// What each G-buffer target holds. `albedo`, `normal` and the channels'
/// `target`s index [`GBuffer::targets`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reading {
    normals: Normals,
    albedo: usize,
    normal: usize,
    roughness: Channel,
    /// The roughness channel holds gloss (roughness is one minus it).
    gloss: bool,
    /// None: no metalness is stored, so F0 is 0.04 everywhere.
    metalness: Option<Channel>,
    /// The albedo is sRGB encoded and has to be decoded in the shader (an
    /// albedo target that is itself sRGB is decoded by its view regardless).
    srgb: bool,
}

/// Why no guides are made when a reading switch holds something else than
/// what is listed in the module doc.
const NOT_UNDERSTOOD: &str = "a VICTORY_RR_NORMALS, VICTORY_RR_GBUFFER or VICTORY_RR_ALBEDO value is not understood";
/// Why `world-xyz` normals are refused.
const WORLD_SPACE: &str = "the normals are world space (VICTORY_RR_NORMALS=world-xyz) and the layer has no camera view matrix to give Ray Reconstruction or to turn them with";

/// A channel as `<i>.<c>`, in lower case.
fn parse_channel(text: &str) -> Option<Channel> {
    let (target, channel) = text.split_once('.')?;
    let target: usize = target.trim().parse().ok().filter(|t| *t < 4)?;
    let channel = match channel.trim() {
        "r" => 0,
        "g" => 1,
        "b" => 2,
        "a" => 3,
        _ => return None,
    };
    Some(Channel { target, channel })
}

/// The reading from the three switches' values (None or empty: the default).
/// Values are trimmed and matched without case.
fn parse_reading(normals: Option<&str>, gbuffer: Option<&str>, albedo: Option<&str>) -> Result<Reading, String> {
    let clean = |v: Option<&str>| v.map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
    let normals = match clean(normals).as_deref() {
        None | Some("view-xyz") => Normals::ViewXyz,
        Some("octahedral") => Normals::Octahedral,
        Some("world-xyz") => Normals::WorldXyz,
        Some(other) => return Err(format!("VICTORY_RR_NORMALS={other}")),
    };
    let srgb = match clean(albedo).as_deref() {
        None | Some("srgb") => true,
        Some("linear") => false,
        Some(other) => return Err(format!("VICTORY_RR_ALBEDO={other}")),
    };
    let text = clean(gbuffer).unwrap_or_else(|| "0,1,2.r,2.g".into());
    let bad = || format!("VICTORY_RR_GBUFFER={text}");
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let [albedo, normal, roughness, metalness] = parts[..] else { return Err(bad()) };
    let index = |p: &str| p.parse::<usize>().ok().filter(|t| *t < 4);
    let (roughness, gloss) = match roughness.strip_prefix("gloss:") {
        Some(rest) => (rest, true),
        None => (roughness, false),
    };
    let metalness = if metalness == "none" { None } else { Some(parse_channel(metalness).ok_or_else(bad)?) };
    Ok(Reading {
        normals,
        albedo: index(albedo).ok_or_else(bad)?,
        normal: index(normal).ok_or_else(bad)?,
        roughness: parse_channel(roughness).ok_or_else(bad)?,
        gloss,
        metalness,
        srgb,
    })
}

static READING: OnceLock<Result<Reading, String>> = OnceLock::new();

/// The reading in use, from the switches, read once (the first call decides;
/// what is wrong with a value is logged then).
fn reading() -> Result<&'static Reading, &'static str> {
    READING
        .get_or_init(|| {
            let var = |name| std::env::var(name).ok();
            let read = parse_reading(
                var("VICTORY_RR_NORMALS").as_deref(),
                var("VICTORY_RR_GBUFFER").as_deref(),
                var("VICTORY_RR_ALBEDO").as_deref(),
            );
            if let Err(e) = &read {
                tracing::warn!("DLSS Ray Reconstruction: {e} is not understood");
            }
            read
        })
        .as_ref()
        .map_err(|_| NOT_UNDERSTOOD)
}

/// The `#define` lines placed in front of [`SOURCE`] for a reading, the way
/// the ambient pass places its parts.
fn defines(r: &Reading) -> String {
    let sample = |c: Channel| format!("target{}.Load(p).{}", c.target, ['r', 'g', 'b', 'a'][c.channel]);
    let roughness = if r.gloss { format!("(1.0 - {})", sample(r.roughness)) } else { sample(r.roughness) };
    let metalness = r.metalness.map_or_else(|| "0.0".to_string(), sample);
    let mut out = format!(
        "#define ALBEDO_TARGET target{}\n#define NORMAL_TARGET target{}\n#define ROUGHNESS(p) {roughness}\n#define METALNESS(p) {metalness}\n",
        r.albedo, r.normal
    );
    if r.normals == Normals::Octahedral {
        out.push_str("#define NORMALS_OCTAHEDRAL 1\n");
    }
    if r.srgb {
        out.push_str("#define ALBEDO_SRGB 1\n");
    }
    out
}

/// The three guide textures of ours and the host state each is left in (a
/// later pass in the list reads them, so not the one they were written in).
pub(crate) struct Guides {
    pub normal_roughness: (ID3D12Resource, u32),
    pub albedo: (ID3D12Resource, u32),
    pub specular_albedo: (ID3D12Resource, u32),
}

/// The compute pass: its root signature (inside the shader) and pipeline.
struct Pass {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// The textures we write, their size, and the descriptor ring.
#[derive(Clone)]
struct Made {
    size: [u32; 2],
    /// Normals with roughness, albedo, specular albedo.
    textures: [ID3D12Resource; 3],
    heap: ID3D12DescriptorHeap,
    step: u32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Made {}
// SAFETY: as above.
unsafe impl Sync for Made {}

/// The motion target DLSS is given at the latest upscaling (0: none yet).
static MOTION_TARGET: AtomicUsize = AtomicUsize::new(0);
/// Draws that wrote the G-buffer since the last upscaling.
static GBUFFER_DRAWS: AtomicU64 = AtomicU64::new(0);
/// The G-buffer the draws wrote last.
static LATEST: Mutex<Option<GBuffer>> = Mutex::new(None);
static PASS: OnceLock<Option<Pass>> = OnceLock::new();
static TEXTURES: Mutex<Option<Made>> = Mutex::new(None);
/// A worker is making the textures.
static MAKING: AtomicBool = AtomicBool::new(false);
/// The size the textures could not be made at (width in the high half), so a
/// failure is said once and not retried every frame; 0 is none.
static FAILED: AtomicU64 = AtomicU64::new(0);
/// The state our textures are in at the end of the last recording.
static STATE: AtomicU32 = AtomicU32::new(UNORDERED);
/// The views the descriptor ring holds now: what they are of, which slot,
/// and the resources themselves (held, so an address in the key cannot be
/// reused by another resource while its views are still in use).
struct Views {
    key: [usize; 8],
    slot: u32,
    _held: Vec<ID3D12Resource>,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Views {}
static VIEWS: Mutex<Option<Views>> = Mutex::new(None);
/// Bumped when the stored G-buffer is dropped for being the wrong size, so
/// each thread describes its set again.
static EPOCH: AtomicU32 = AtomicU32::new(0);
/// Why [`pick`] refuses a G-buffer of another size than the render's.
const NOT_RENDER_SIZE: &str = "the G-buffer is not the render size";

thread_local! {
    /// The set this thread last looked at (and the epoch it did so in), so the
    /// same set again (every draw of a frame, nearly) takes no lock and
    /// describes nothing.
    static LAST: Cell<([usize; 4], u32)> = const { Cell::new(([0; 4], 0)) };
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether Ray Reconstruction is on. Unset, every entry point returns after
/// this one load.
pub(crate) fn on() -> bool {
    super::RR_READY.load(Relaxed)
}

/// An upscaling is being recorded (the title's thread, at the dispatch):
/// the motion target is what finds the G-buffer's draws.
pub(crate) fn upscaling(up: &Upscale) {
    if !on() {
        return;
    }
    MOTION_TARGET.store(up.resources[MOTION].0 as usize, Relaxed);
}

/// The first four render targets, in bound order, that are neither empty nor
/// the motion target.
fn first_four(targets: &[usize], motion: usize) -> Option<[usize; 4]> {
    let mut found = targets.iter().copied().filter(|&t| t != 0 && t != motion);
    Some([found.next()?, found.next()?, found.next()?, found.next()?])
}

/// Whether four described targets are a G-buffer as run 17 showed it: the
/// first an 8-bit colour format of any of typeless, unorm or sRGB (27 to 29;
/// unorm is read as sRGB-encoded data by the shader), the other three unorm (28),
/// every one the motion target's size.
fn accept(targets: [usize; 4], motion_size: [u32; 2], seen: [([u32; 2], i32); 4]) -> Option<GBuffer> {
    let layout = (27..=29).contains(&seen[0].1) && seen[1..].iter().all(|s| s.1 == 28);
    let same = seen.iter().all(|s| s.0 == motion_size);
    (layout && same).then(|| GBuffer { targets, size: motion_size, formats: seen.map(|s| s.1) })
}

/// A draw or dispatch is being recorded with `targets` bound (any recording
/// thread, from the draw path, which sees every draw; the compute path sees
/// a table only where it is walked). Draws that also write the motion target
/// are counted, and the G-buffer they write is kept when it changes.
pub(crate) fn pass(compute: bool, targets: &[usize]) {
    if !on() || compute || targets.len() < 5 || super::RR_OFF.load(Relaxed) {
        return;
    }
    let motion = MOTION_TARGET.load(Relaxed);
    if motion == 0 || !targets.contains(&motion) {
        return;
    }
    GBUFFER_DRAWS.fetch_add(1, Relaxed);
    let Some(set) = first_four(targets, motion) else { return };
    let epoch = EPOCH.load(Relaxed);
    if LAST.with(|l| l.get() == (set, epoch)) {
        return;
    }
    // described outside any lock; a texture not made yet has no description
    // and is asked again at the next draw
    let Some((motion_size, _)) = super::targets::describe(motion) else { return };
    let mut seen = [([0u32; 2], 0i32); 4];
    for (to, &t) in seen.iter_mut().zip(&set) {
        let Some(d) = super::targets::describe(t) else { return };
        *to = d;
    }
    // a set that was looked at and is not a G-buffer is not looked at again
    LAST.with(|l| l.set((set, epoch)));
    let Some(g) = accept(set, motion_size, seen) else { return };
    let before = lock(&LATEST).replace(g.clone());
    if before.as_ref() != Some(&g) {
        tracing::info!(
            "DLSS Ray Reconstruction: the G-buffer is the render targets of the draws that also write the motion vectors, in bound order: [0] {:#x} f{}, [1] {:#x} f{}, [2] {:#x} f{}, [3] {:#x} f{} ({}x{}); what each holds is the reading line's",
            g.targets[0],
            g.formats[0],
            g.targets[1],
            g.formats[1],
            g.targets[2],
            g.formats[2],
            g.targets[3],
            g.formats[3],
            g.size[0],
            g.size[1]
        );
    }
}

/// The device is up and Ray Reconstruction loaded: compiles the pass on a
/// worker, so neither the compile nor the pipeline is made on a thread the
/// title waits on.
pub(crate) fn prepare() {
    if !on() {
        return;
    }
    let r = match reading() {
        Ok(r) => *r,
        Err(e) => {
            tracing::warn!("DLSS Ray Reconstruction: {e}; Super Resolution does every upscaling");
            let _ = PASS.set(None);
            return;
        }
    };
    let name = |c: Channel| format!("{}.{}", c.target, ['r', 'g', 'b', 'a'][c.channel]);
    tracing::info!(
        "DLSS Ray Reconstruction: the G-buffer is read as: albedo target {} ({}), normals target {} ({}), roughness {}{}, metalness {} (VICTORY_RR_GBUFFER, VICTORY_RR_NORMALS, VICTORY_RR_ALBEDO; defaults are a reading of formats and bind order, not a confirmed one)",
        r.albedo,
        if r.srgb { "srgb" } else { "linear" },
        r.normal,
        match r.normals {
            Normals::ViewXyz => "view-xyz",
            Normals::Octahedral => "octahedral",
            Normals::WorldXyz => "world-xyz",
        },
        name(r.roughness),
        if r.gloss { " (gloss)" } else { "" },
        r.metalness.map_or_else(|| "none".to_string(), name)
    );
    let spawned = std::thread::Builder::new().name("victory-guides".into()).spawn(move || {
        let built = build(&r);
        match &built {
            Ok(_) => tracing::info!("DLSS Ray Reconstruction: the guide pass is built"),
            Err(e) => tracing::warn!(error = %e, "DLSS Ray Reconstruction: the guide pass could not be built; Super Resolution does every upscaling"),
        }
        // (a second prepare finds it set and leaves it)
        let _ = PASS.set(built.ok());
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "DLSS Ray Reconstruction: no thread for the guide pass");
    }
}

fn build(r: &Reading) -> windows::core::Result<Pass> {
    let host = host().ok_or_else(windows::core::Error::empty)?;
    let code = crate::host::compile(&format!("{}{SOURCE}", defines(r)), s!("main"), s!("cs_5_1"))?;
    // SAFETY: the root signature is inside the shader; plain creations.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code)?;
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
            ..Default::default()
        };
        let pipeline = host.device.CreateComputePipelineState(&desc)?;
        Ok(Pass { root, pipeline })
    }
}

/// The steps of [`make`] that need no list: a draw wrote the G-buffer since
/// the last upscaling, one is known and is the render size, and the colour
/// is high dynamic range.
fn pick(draws: u64, latest: Option<GBuffer>, render: [u32; 2], flags: u32) -> Result<GBuffer, &'static str> {
    if draws == 0 {
        return Err("no draw wrote the G-buffer since the last upscaling");
    }
    let set = latest.ok_or("no G-buffer found yet")?;
    if set.size != render {
        return Err(NOT_RENDER_SIZE);
    }
    if flags & plan::FFX_HIGH_DYNAMIC_RANGE == 0 {
        return Err("the colour is not high dynamic range; Ray Reconstruction takes only that");
    }
    Ok(set)
}

/// The textures at `size`, on a worker (a big allocation never happens on a
/// recording thread). Until they are there [`make`] answers that they are
/// being made.
fn make_textures(size: [u32; 2]) {
    if MAKING.compare_exchange(false, true, AcqRel, Relaxed).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("victory-guides-textures".into()).spawn(move || {
        match textures(size) {
            Ok(made) => {
                STATE.store(UNORDERED, Relaxed);
                *lock(&TEXTURES) = Some(made);
                tracing::info!("DLSS Ray Reconstruction: the guide textures are made ({}x{})", size[0], size[1]);
            }
            Err(e) => {
                FAILED.store(u64::from(size[0]) << 32 | u64::from(size[1]), Relaxed);
                tracing::warn!(error = %e, "DLSS Ray Reconstruction: the guide textures could not be made; Super Resolution does every upscaling");
            }
        }
        MAKING.store(false, Release);
    });
    if spawned.is_err() {
        MAKING.store(false, Release);
    }
}

fn textures(size: [u32; 2]) -> windows::core::Result<Made> {
    let host = host().ok_or_else(windows::core::Error::empty)?;
    let make = |format| super::texture(&host.device, size, format, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, UNORDERED);
    let textures = [
        make(DXGI_FORMAT_R16G16B16A16_FLOAT)?,
        make(DXGI_FORMAT_R11G11B10_FLOAT)?,
        make(DXGI_FORMAT_R11G11B10_FLOAT)?,
    ];
    // SAFETY: plain creations; a heap of the kind compose.rs makes for its views.
    let (heap, step) = unsafe {
        let heap: ID3D12DescriptorHeap = host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: SLOTS * FRAMES,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        (heap, host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV))
    };
    Ok(Made { size, textures, heap, step })
}

/// A view the pass reads: `format` of `texture` at CPU handle `at`.
fn read_view(device: &ID3D12Device, texture: &ID3D12Resource, format: DXGI_FORMAT, at: usize) {
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
        },
    };
    // SAFETY: a slot of our heap that no list in flight reads (see FRAMES); a live texture.
    unsafe { device.CreateShaderResourceView(texture, Some(&desc), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: at }) };
}

/// A view the pass writes: our texture, in its own format.
fn write_view(device: &ID3D12Device, texture: &ID3D12Resource, at: usize) {
    // SAFETY: a plain query.
    let format = unsafe { texture.GetDesc() }.Format;
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV::default() },
    };
    // SAFETY: a slot of our heap that no list in flight reads (see FRAMES); a live texture.
    unsafe {
        device.CreateUnorderedAccessView(texture, None::<&ID3D12Resource>, Some(&desc), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: at })
    };
}

/// The view format the pass reads the depth in: the depth plane of a
/// typeless combined format, or the 32-bit float of a typeless or colour one.
/// A fully typed depth format takes no shader view of another format, so
/// those are left to super resolution.
fn depth_view(format: i32) -> Option<DXGI_FORMAT> {
    match format {
        // R32G8X24_TYPELESS
        19 => Some(DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS),
        // R32_TYPELESS, R32_FLOAT
        39 | 41 => Some(DXGI_FORMAT_R32_FLOAT),
        _ => None,
    }
}

/// Records the conversion pass into `host`, the title's list, ahead of the
/// evaluation: the title's G-buffer and the depth DLSS is given in, our three
/// guide textures out. Runs inside `list::external`, on the title's thread,
/// where the title's own list is recorded; the list's state is replayed after
/// it. Why there are no guides this time is the `Err` (the caller logs each
/// reason once and super resolution takes the upscaling).
pub(crate) fn make(host: &HostList, up: &Upscale, depth: &(ID3D12Resource, u32)) -> Result<Guides, &'static str> {
    if !on() {
        return Err("Ray Reconstruction is off");
    }
    let r = reading()?;
    if r.normals == Normals::WorldXyz {
        return Err(WORLD_SPACE);
    }
    let camera = up.camera.planes_in_order();
    // the pass reads the depth in a compute shader, so DLSS must have been
    // given it in a state that allows that (not the pixel-read one)
    if depth.1 & NON_PIXEL_READ == 0 {
        return Err("the depth is not readable by a compute shader");
    }
    let set = match pick(GBUFFER_DRAWS.swap(0, Relaxed), lock(&LATEST).clone(), up.camera.render, up.camera.flags) {
        Ok(set) => set,
        Err(why) => {
            if why == NOT_RENDER_SIZE {
                // the title may have made its targets again at a new size
                // under the same addresses, which the threads' memory of the
                // set would never look at again: forget it
                *lock(&LATEST) = None;
                EPOCH.fetch_add(1, Relaxed);
            }
            return Err(why);
        }
    };
    let pass = PASS.get().and_then(Option::as_ref).ok_or("the guide pass is not built yet")?;
    let device = &crate::host::host().ok_or("no host device")?.device;
    // the textures, made off this thread when missing or of another size
    let made = lock(&TEXTURES).clone().filter(|m| m.size == set.size);
    let Some(made) = made else {
        if FAILED.load(Relaxed) == u64::from(set.size[0]) << 32 | u64::from(set.size[1]) {
            return Err("the guide textures could not be made");
        }
        make_textures(set.size);
        return Err("the guide textures are being made");
    };
    // the rays of the camera: the inverse of its projection gives, for a
    // pixel, the direction from the eye, which N.V needs (view space)
    if !(camera.fov_y > 0.0 && camera.fov_y < std::f32::consts::PI) || !(camera.near > 0.0) {
        return Err("the camera has no usable field of view");
    }
    let aspect = camera.render[0] as f32 / camera.render[1].max(1) as f32;
    let projection = plan::projection(camera.near, camera.far, camera.fov_y, aspect, camera.depth_inverted());
    let inverse = plan::invert(&projection).ok_or("the camera's projection cannot be inverted")?;
    let rays = [inverse[0][0], inverse[1][1]];
    if !rays.iter().all(|r| r.is_finite() && *r > 0.0) {
        return Err("the camera's projection cannot be inverted");
    }
    // the title's targets as a compute shader reads them
    let mut reads: [Option<ID3D12Resource>; 4] = Default::default();
    for (to, &t) in reads.iter_mut().zip(&set.targets) {
        let t = t as This;
        if crate::external::prepare_input(host, t).is_none() {
            return Err("a G-buffer target cannot be read by a shader");
        }
        match crate::external::host_texture(host, t, SHADER_READ) {
            Some((texture, state)) if state == SHADER_READ => *to = Some(texture),
            _ => return Err("a G-buffer target cannot be read by a shader"),
        }
    }
    let [Some(a), Some(b), Some(c), Some(d)] = reads else {
        return Err("a G-buffer target cannot be read by a shader");
    };
    // SAFETY: a plain query.
    let depth_format = unsafe { depth.0.GetDesc() }.Format.0;
    let depth_format = depth_view(depth_format).ok_or("the depth has a format the guide pass cannot read")?;
    // a target is read through sRGB when it is that format (the shader then
    // does not decode it again), otherwise as unorm
    let view = |i: usize| if set.formats[i] == 29 { DXGI_FORMAT_R8G8B8A8_UNORM_SRGB } else { DXGI_FORMAT_R8G8B8A8_UNORM };
    let reads = [(&a, view(0)), (&b, view(1)), (&c, view(2)), (&d, view(3)), (&depth.0, depth_format)];
    // which of the four are read as sRGB, one bit each
    let srgb_views = (0..4).fold(0usize, |m, i| m | usize::from(set.formats[i] == 29) << i);
    // views in a slot of the ring: targets and depth (t0..t4), then ours
    // (u0..u2). They are the same every frame, so they are written only when
    // what they are of changes, into the next slot, and a list in flight is
    // never read from a slot being written
    let key = [
        a.as_raw() as usize,
        b.as_raw() as usize,
        c.as_raw() as usize,
        d.as_raw() as usize,
        depth.0.as_raw() as usize,
        made.heap.as_raw() as usize,
        srgb_views,
        depth_format.0 as usize,
    ];
    let (slot, written) = {
        let mut views = lock(&VIEWS);
        match views.as_ref() {
            Some(v) if v.key == key => (v.slot, false),
            old => {
                // another heap starts at its first slot
                let next = old.filter(|v| v.key[5] == key[5]).map_or(0, |v| (v.slot + 1) % FRAMES);
                let held = reads.iter().map(|(t, _)| (*t).clone()).collect();
                *views = Some(Views { key, slot: next, _held: held });
                (next, true)
            }
        }
    };
    // SAFETY: a plain query of a live heap.
    let (cpu, gpu) = unsafe { (made.heap.GetCPUDescriptorHandleForHeapStart(), made.heap.GetGPUDescriptorHandleForHeapStart()) };
    let first = (slot * SLOTS) as usize * made.step as usize;
    let at = |i: usize| cpu.ptr + first + i * made.step as usize;
    if written {
        for (i, (texture, format)) in reads.into_iter().enumerate() {
            read_view(device, texture, format, at(i));
        }
        for (i, texture) in made.textures.iter().enumerate() {
            write_view(device, texture, at(5 + i));
        }
    }
    host.keep(&made.heap);
    for texture in &made.textures {
        host.keep(texture);
    }
    let words: [u32; 6] = [
        set.size[0],
        set.size[1],
        rays[0].to_bits(),
        rays[1].to_bits(),
        u32::from(camera.depth_inverted()),
        u32::from(set.formats[r.albedo] == 29),
    ];
    // ours go from what the last list left them in to writable, and after
    // the pass to readable by the evaluation (which is told 0x40)
    let was = STATE.swap(NON_PIXEL_READ, Relaxed);
    let states = |from: u32, to: u32| {
        made.textures.each_ref().map(|t| crate::host::transition(t, D3D12_RESOURCE_STATES(from as i32), D3D12_RESOURCE_STATES(to as i32)))
    };
    let uav_barrier = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
    let l = &host.list;
    // the depth is in a compute-readable state (checked above), the targets
    // were moved to the shader-read state by the calls above
    // SAFETY: an open list; our heap and textures are held by it; the root signature declares the six constants and two tables set here.
    unsafe {
        if was != UNORDERED {
            l.ResourceBarrier(&states(was, UNORDERED));
        }
        l.SetDescriptorHeaps(&[Some(made.heap.clone())]);
        l.SetComputeRootSignature(&pass.root);
        l.SetPipelineState(&pass.pipeline);
        l.SetComputeRoot32BitConstants(0, 6, words.as_ptr().cast(), 0);
        let table = |i: usize| D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu.ptr + (first + i * made.step as usize) as u64 };
        l.SetComputeRootDescriptorTable(1, table(0));
        l.SetComputeRootDescriptorTable(2, table(5));
        l.Dispatch(set.size[0].div_ceil(8), set.size[1].div_ceil(8), 1);
        l.ResourceBarrier(&[uav_barrier]);
        l.ResourceBarrier(&states(UNORDERED, NON_PIXEL_READ));
    }
    let [normal_roughness, albedo, specular_albedo] = made.textures.map(|t| (t, NON_PIXEL_READ));
    Ok(Guides { normal_roughness, albedo, specular_albedo })
}

/// One compute entry over 8x8 pixels, placed after the lines [`defines`]
/// writes for the reading. Root constants: width, height, the camera's
/// inverse projection's x and y terms, whether the depth is inverted, whether
/// the albedo target is read through an sRGB view (and so already linear).
///
/// The reading of the title's targets is from formats and bind order only,
/// until the pictures of a run confirm it (see [`reading`]); the helpers at
/// the top are the only places that depend on it. A world-space normal would
/// need the camera's view rotation, which the layer does not have.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), DescriptorTable(SRV(t0, numDescriptors=5)), DescriptorTable(UAV(u0, numDescriptors=3))"
cbuffer Set : register(b0) {
    uint width; uint height; float ray_x; float ray_y; uint inverted; uint decoded;
};
Texture2D<float4> target0 : register(t0);
Texture2D<float4> target1 : register(t1);
Texture2D<float4> target2 : register(t2);
Texture2D<float4> target3 : register(t3);
Texture2D<float> depth : register(t4);
RWTexture2D<float4> normal_roughness : register(u0);
RWTexture2D<float4> albedo_out : register(u1);
RWTexture2D<float4> specular_out : register(u2);

// the reading of the title's targets (see the note above the source); the
// normals point toward the eye in view space, so +z faces the camera
float3 decode_normal(float4 t) {
#ifdef NORMALS_OCTAHEDRAL
    float2 f = t.xy * 2.0 - 1.0;
    float3 n = float3(f, 1.0 - abs(f.x) - abs(f.y));
    float w = saturate(-n.z);
    n.xy += (n.xy >= 0.0) ? -w : w;
#else
    float3 n = t.xyz * 2.0 - 1.0;
#endif
    return dot(n, n) > 1e-6 ? normalize(n) : float3(0, 0, 1);
}
float3 decode_albedo(float4 t) {
#ifdef ALBEDO_SRGB
    float3 lo = t.rgb / 12.92;
    float3 hi = pow(max((t.rgb + 0.055) / 1.055, 0.0), 2.4);
    // already linear when the view did the decoding
    return decoded != 0 ? t.rgb : lerp(lo, hi, step(0.04045, t.rgb));
#else
    return t.rgb;
#endif
}

// the guide's approximation of the specular albedo (Ray Tracing Gems, ch. 32)
float3 EnvBRDFApprox2(float3 SpecularColor, float alpha, float NoV) {
    NoV = abs(NoV);
    float4 X; X.x = 1.0; X.y = NoV; X.z = NoV * NoV; X.w = NoV * X.z;
    float4 Y; Y.x = 1.0; Y.y = alpha; Y.z = alpha * alpha; Y.w = alpha * Y.z;
    float2x2 M1 = float2x2(0.99044, -1.28514, 1.29678, -0.755907);
    float3x3 M2 = float3x3(1.0, 2.92338, 59.4188, 20.3225, -27.0302, 222.592, 121.563, 626.13, 316.627);
    float2x2 M3 = float2x2(0.0365463, 3.32707, 9.0632, -9.04756);
    float3x3 M4 = float3x3(1.0, 3.59685, -1.36772, 9.04401, -16.3174, 9.22949, 5.56589, 19.7886, -20.2123);
    float bias = dot(mul(M1, X.xy), Y.xy) * rcp(dot(mul(M2, X.xyw), Y.xyw));
    float scale = dot(mul(M3, X.xy), Y.xy) * rcp(dot(mul(M4, X.xzw), Y.xyw));
    bias *= saturate(SpecularColor.g * 50.0);
    return mad(SpecularColor, max(0.0, scale), max(0.0, bias));
}

[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 at : SV_DispatchThreadID) {
    if (at.x >= width || at.y >= height) return;
    int3 p = int3(at.xy, 0);
    float z = depth.Load(p);
    // nothing is there at the far end of the depth: a neutral surface that
    // reflects nothing, which the sky's pixels are not guided by
    bool empty = inverted != 0 ? z <= 0.0 : z >= 1.0;
    float3 n = float3(0, 0, 1);
    float rough = 1.0;
    float3 base = float3(0, 0, 0);
    float3 spec = float3(0, 0, 0);
    if (!empty) {
        n = decode_normal(NORMAL_TARGET.Load(p));
        rough = saturate(ROUGHNESS(p));
        float metal = saturate(METALNESS(p));
        base = decode_albedo(ALBEDO_TARGET.Load(p));
        // the direction to the eye in view space: +z points toward the eye,
        // so the ray through the pixel (which looks along -z here) is turned round
        float2 ndc = float2((at.x + 0.5) / width * 2.0 - 1.0, 1.0 - (at.y + 0.5) / height * 2.0);
        float3 v = normalize(float3(-ndc.x * ray_x, -ndc.y * ray_y, 1.0));
        // reflectance at normal incidence: 4% for dielectrics, the albedo for metals
        float3 f0 = lerp(float3(0.04, 0.04, 0.04), base, metal);
        spec = EnvBRDFApprox2(f0, rough * rough, dot(n, v));
        // the guide asks for the diffuse part of the reflectance, and a metal has none
        base *= 1.0 - metal;
    }
    normal_roughness[at.xy] = float4(n, rough);
    albedo_out[at.xy] = float4(base, 1);
    specular_out[at.xy] = float4(spec, 1);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const MOTION_SIZE: [u32; 2] = [64, 36];

    fn seen(formats: [i32; 4], size: [u32; 2]) -> [([u32; 2], i32); 4] {
        formats.map(|f| (size, f))
    }

    #[test]
    fn a_set_needs_the_layout_and_the_size() {
        let set = [1, 2, 3, 4];
        let ok = accept(set, MOTION_SIZE, seen([27, 28, 28, 28], MOTION_SIZE));
        assert_eq!(ok, Some(GBuffer { targets: set, size: MOTION_SIZE, formats: [27, 28, 28, 28] }));
        assert!(accept(set, MOTION_SIZE, seen([29, 28, 28, 28], MOTION_SIZE)).is_some());
        // the first may be 27 to 29 only, the others unorm only
        assert!(accept(set, MOTION_SIZE, seen([28, 28, 28, 28], MOTION_SIZE)).is_some());
        assert!(accept(set, MOTION_SIZE, seen([30, 28, 28, 28], MOTION_SIZE)).is_none());
        assert!(accept(set, MOTION_SIZE, seen([27, 28, 29, 28], MOTION_SIZE)).is_none());
        // another size than the motion target's
        assert!(accept(set, MOTION_SIZE, seen([27, 28, 28, 28], [32, 18])).is_none());
        let mut one_off = seen([27, 28, 28, 28], MOTION_SIZE);
        one_off[3].0 = [64, 35];
        assert!(accept(set, MOTION_SIZE, one_off).is_none());
    }

    #[test]
    fn the_first_four_skip_the_motion_target_and_empty_slots() {
        assert_eq!(first_four(&[10, 11, 0, 12, 99, 13, 14], 99), Some([10, 11, 12, 13]));
        assert_eq!(first_four(&[10, 11, 12, 99], 99), None);
        assert_eq!(first_four(&[], 99), None);
    }

    #[test]
    fn the_same_set_again_takes_no_description() {
        // the thread's memory of the last set is what the draw path checks
        // before it describes anything
        let set = [1usize, 2, 3, 4];
        assert!(!LAST.with(|l| l.get() == (set, 0)));
        LAST.with(|l| l.set((set, 0)));
        assert!(LAST.with(|l| l.get() == (set, 0)));
        assert!(!LAST.with(|l| l.get() == (set, 1)));
        assert!(!LAST.with(|l| l.get() == ([1, 2, 3, 5], 0)));
    }

    #[test]
    fn without_a_set_there_is_a_reason() {
        let hdr = plan::FFX_HIGH_DYNAMIC_RANGE;
        let set = GBuffer { targets: [1, 2, 3, 4], size: MOTION_SIZE, formats: [27, 28, 28, 28] };
        assert_eq!(pick(0, Some(set.clone()), MOTION_SIZE, hdr), Err("no draw wrote the G-buffer since the last upscaling"));
        assert_eq!(pick(3, None, MOTION_SIZE, hdr), Err("no G-buffer found yet"));
        assert_eq!(pick(3, Some(set.clone()), [128, 72], hdr), Err(NOT_RENDER_SIZE));
        assert_eq!(
            pick(3, Some(set.clone()), MOTION_SIZE, 0),
            Err("the colour is not high dynamic range; Ray Reconstruction takes only that")
        );
        assert_eq!(pick(3, Some(set.clone()), MOTION_SIZE, hdr), Ok(set));
    }

    fn view_gloss_none_linear() -> Reading {
        Reading {
            normals: Normals::Octahedral,
            albedo: 3,
            normal: 0,
            roughness: Channel { target: 1, channel: 3 },
            gloss: true,
            metalness: None,
            srgb: false,
        }
    }

    #[test]
    fn the_reading_defaults() {
        let r = parse_reading(None, None, None).unwrap();
        assert_eq!(
            r,
            Reading {
                normals: Normals::ViewXyz,
                albedo: 0,
                normal: 1,
                roughness: Channel { target: 2, channel: 0 },
                gloss: false,
                metalness: Some(Channel { target: 2, channel: 1 }),
                srgb: true,
            }
        );
        // empty values are the defaults too
        assert_eq!(parse_reading(Some(" "), Some(""), Some("")), Ok(r));
    }

    #[test]
    fn the_reading_values() {
        assert_eq!(parse_reading(Some(" Octahedral "), None, None).unwrap().normals, Normals::Octahedral);
        assert_eq!(parse_reading(Some("world-xyz"), None, None).unwrap().normals, Normals::WorldXyz);
        assert_eq!(parse_reading(Some("octahedral"), Some("3,0,gloss:1.a,none"), Some("LINEAR")), Ok(view_gloss_none_linear()));
        assert!(parse_reading(Some("view"), None, None).is_err());
        assert!(parse_reading(None, Some("4,1,2.r,2.g"), None).is_err());
        assert!(parse_reading(None, Some("0,1,2.x,2.g"), None).is_err());
        assert!(parse_reading(None, Some("0,1,2.r"), None).is_err());
        assert!(parse_reading(None, None, Some("gamma")).is_err());
    }

    #[test]
    fn the_defines_follow_the_reading() {
        let d = defines(&parse_reading(None, None, None).unwrap());
        assert!(d.contains("#define ALBEDO_TARGET target0\n"));
        assert!(d.contains("#define ROUGHNESS(p) target2.Load(p).r\n"));
        assert!(d.contains("#define METALNESS(p) target2.Load(p).g\n"));
        assert!(d.contains("#define ALBEDO_SRGB 1\n"));
        assert!(!d.contains("NORMALS_OCTAHEDRAL"));
        let d = defines(&view_gloss_none_linear());
        assert!(d.contains("#define ROUGHNESS(p) (1.0 - target1.Load(p).a)\n"));
        assert!(d.contains("#define METALNESS(p) 0.0\n"));
        assert!(d.contains("#define NORMALS_OCTAHEDRAL 1\n"));
        assert!(!d.contains("ALBEDO_SRGB"));
    }

    #[test]
    fn the_pass_compiles() {
        // the shader compiler needs no device
        for r in [parse_reading(None, None, None).unwrap(), view_gloss_none_linear()] {
            let code = crate::host::compile(&format!("{}{SOURCE}", defines(&r)), s!("main"), s!("cs_5_1"));
            assert!(code.is_ok_and(|c| !c.is_empty()), "{r:?}");
        }
    }

    #[test]
    fn the_depth_views() {
        assert_eq!(depth_view(19), Some(DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS));
        assert_eq!(depth_view(39), Some(DXGI_FORMAT_R32_FLOAT));
        assert_eq!(depth_view(20), None);
        assert_eq!(depth_view(40), None);
        assert_eq!(depth_view(13), None);
    }
}
