//! 3D model attachments: STL/PLY parsing into damascene scene geometry,
//! an offscreen-rendered poster thumbnail, and the interactive orbit
//! lightbox.
//!
//! Mirrors [`crate::video`]'s shape: extension detection, a parsed-model
//! cache feeding a static poster thumbnail, and a lightbox the user can
//! manipulate. The difference is *who decodes*. libmpv hands video back
//! as pixels; damascene has no 3D file loader — its `chart3d` element
//! takes in-memory geometry ([`MeshData`] / [`PointData`]), so we parse
//! the file into those types ourselves and let damascene render them.
//!
//! Format coverage is deliberately narrow and matched to what an
//! untextured, lit-mesh renderer can show without loss:
//!
//! - **STL** — triangle soup with per-face normals. Flat-shaded (each
//!   face expands to three unique vertices carrying the face normal).
//!   STL files frequently store zeroed normals, so we recompute from the
//!   winding when the stored normal is degenerate.
//! - **PLY** — dual-natured. With faces it loads as a mesh (smooth
//!   per-vertex normals computed when absent); vertices-only loads as a
//!   coloured point cloud, which damascene renders natively.
//!
//! ## Thumbnail vs lightbox
//!
//! The inline chat card shows a *static* poster: [`ModelThumbnailer`]
//! renders the scene once via an offscreen [`damascene_wgpu::Runner`]
//! and reads the pixels back into an [`Image`] (the chat-side analogue of
//! libmpv's first-frame poster). Clicking the card opens a lightbox that
//! projects a live `chart3d` element keyed for camera persistence, so
//! damascene's built-in orbit/pan/zoom drives the view — no per-frame
//! state of our own beyond the parsed geometry.

use std::{fs::File, io::BufReader, path::Path};

use damascene_core::{
    prelude::*,
    scene::{
        CameraControls, Framing, Material, MeshData, MeshHandle, MeshVertex, PointData, PointStyle, PointsHandle,
        ScenePoint, SceneSpec, glam::Vec3,
    },
};
use damascene_wgpu::Runner;

// ---- Routing keys ----

pub const KEY_LIGHTBOX_DISMISS: &str = "model:lightbox:dismiss";
pub const KEY_LIGHTBOX_CLOSE: &str = "model:lightbox:close";
/// Key on the live `chart3d` element. Stable so damascene persists the
/// orbit camera pose in `UiState` across rebuilds (drag-to-orbit only
/// works if the camera survives the per-frame tree rebuild).
pub const KEY_SCENE: &str = "model:lightbox:scene";

/// Routed key for "open this transfer in the model lightbox", used on the
/// inline preview card and the "View 3D" button on a completed download.
pub fn open_model_key(transfer_id: &str) -> String {
    format!("model:open:{transfer_id}")
}

pub fn parse_open_model_key(key: &str) -> Option<&str> {
    key.strip_prefix("model:open:")
}

// ---- Format detection ----

/// True when `name`'s extension is a 3D model format we can parse for an
/// inline preview. Whitelist (mirrors [`crate::video::is_video_name`]):
/// only STL and PLY today.
pub fn is_model_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "stl" | "ply")
}

// ---- Parsing ----

/// Why a model failed to load. Logged, not surfaced to the user — a parse
/// failure just leaves the plain file card in place (no preview swap),
/// exactly like a video whose thumbnail never decodes.
#[derive(Debug)]
pub enum ModelError {
    Io(std::io::Error),
    Parse(String),
    /// Parsed fine but carried no drawable geometry (no faces / no points).
    Empty,
    Unsupported(String),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::Io(e) => write!(f, "io: {e}"),
            ModelError::Parse(m) => write!(f, "parse: {m}"),
            ModelError::Empty => write!(f, "no drawable geometry"),
            ModelError::Unsupported(ext) => write!(f, "unsupported extension: {ext}"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Parsed, app-owned geometry ready to drop into a [`SceneSpec`]. Cloning
/// is a cheap `Arc` bump on the underlying [`damascene_core::scene::GeometryHandle`]
/// — never a geometry copy — so the lightbox and thumbnailer share one
/// upload.
#[derive(Clone)]
pub enum LoadedModel {
    Mesh(MeshHandle),
    Points(PointsHandle),
}

/// Parse `path` into [`LoadedModel`]. Dispatches on extension. Intended to
/// run on `runtime.spawn_blocking` — large meshes take real CPU time.
pub fn load_model(path: &Path) -> Result<LoadedModel, ModelError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "stl" => load_stl(path),
        "ply" => load_ply(path),
        other => Err(ModelError::Unsupported(other.to_string())),
    }
}

fn load_stl(path: &Path) -> Result<LoadedModel, ModelError> {
    let mut reader = BufReader::new(File::open(path).map_err(ModelError::Io)?);
    let mesh = stl_io::read_stl(&mut reader).map_err(ModelError::Io)?;
    if mesh.faces.is_empty() {
        return Err(ModelError::Empty);
    }

    // Flat shading: expand each indexed face to three unique vertices that
    // all carry the face normal. Recompute the normal from the winding
    // when STL's stored one is zeroed (common — many exporters emit
    // `0 0 0` and expect the consumer to derive it).
    let mut vertices = Vec::with_capacity(mesh.faces.len() * 3);
    for face in &mesh.faces {
        let pos = |i: usize| {
            let v = mesh.vertices[face.vertices[i]];
            Vec3::new(v[0], v[1], v[2])
        };
        let (a, b, c) = (pos(0), pos(1), pos(2));
        let stored = Vec3::new(face.normal[0], face.normal[1], face.normal[2]);
        let normal = if stored.length_squared() > 1e-12 {
            stored.normalize()
        } else {
            (b - a).cross(c - a).normalize_or_zero()
        };
        for p in [a, b, c] {
            vertices.push(MeshVertex { position: p, normal });
        }
    }

    Ok(LoadedModel::Mesh(MeshHandle::new(MeshData {
        vertices,
        indices: None,
    })))
}

fn load_ply(path: &Path) -> Result<LoadedModel, ModelError> {
    use ply_rs::{
        parser::Parser,
        ply::{DefaultElement, Property},
    };

    /// Widening scalar accessor: pull `key` from a vertex element as f32
    /// regardless of the file's stored numeric type. Missing → 0.
    #[inline]
    fn prop_f32(el: &DefaultElement, key: &str) -> f32 {
        match el.get(key) {
            Some(Property::Float(v)) => *v,
            Some(Property::Double(v)) => *v as f32,
            Some(Property::Char(v)) => *v as f32,
            Some(Property::UChar(v)) => *v as f32,
            Some(Property::Short(v)) => *v as f32,
            Some(Property::UShort(v)) => *v as f32,
            Some(Property::Int(v)) => *v as f32,
            Some(Property::UInt(v)) => *v as f32,
            _ => 0.0,
        }
    }

    /// Colour-channel accessor: UChar passes through; float channels are
    /// assumed 0..1 and scaled. Missing → mid grey.
    #[inline]
    fn prop_u8(el: &DefaultElement, key: &str) -> u8 {
        match el.get(key) {
            Some(Property::UChar(v)) => *v,
            Some(Property::Char(v)) => *v as u8,
            Some(Property::Float(v)) => (*v * 255.0).round().clamp(0.0, 255.0) as u8,
            Some(Property::Double(v)) => (*v * 255.0).round().clamp(0.0, 255.0) as u8,
            Some(Property::Int(v)) => (*v).clamp(0, 255) as u8,
            Some(Property::UInt(v)) => (*v).min(255) as u8,
            _ => 200,
        }
    }

    /// Read the face element's index lists and triangulate each polygon as
    /// a fan. Accepts the common `vertex_indices` / `vertex_index` names
    /// and any list integer type.
    fn collect_faces(faces: &[DefaultElement]) -> Vec<[u32; 3]> {
        let mut out = Vec::new();
        for face in faces {
            let idx = face.get("vertex_indices").or_else(|| face.get("vertex_index"));
            let poly: Vec<u32> = match idx {
                Some(Property::ListUInt(v)) => v.clone(),
                Some(Property::ListInt(v)) => v.iter().map(|i| *i as u32).collect(),
                Some(Property::ListUChar(v)) => v.iter().map(|i| *i as u32).collect(),
                Some(Property::ListUShort(v)) => v.iter().map(|i| *i as u32).collect(),
                Some(Property::ListShort(v)) => v.iter().map(|i| *i as u32).collect(),
                Some(Property::ListChar(v)) => v.iter().map(|i| *i as u32).collect(),
                _ => continue,
            };
            for i in 1..poly.len().saturating_sub(1) {
                out.push([poly[0], poly[i], poly[i + 1]]);
            }
        }
        out
    }

    let mut reader = BufReader::new(File::open(path).map_err(ModelError::Io)?);
    let parser = Parser::<DefaultElement>::new();
    let ply = parser.read_ply(&mut reader).map_err(ModelError::Io)?;

    let verts = ply
        .payload
        .get("vertex")
        .ok_or_else(|| ModelError::Parse("PLY has no `vertex` element".into()))?;
    if verts.is_empty() {
        return Err(ModelError::Empty);
    }

    // Positions are required; normals and colours are optional. PLY uses
    // varied property names/types across exporters, so pull each scalar
    // through a widening accessor.
    let positions: Vec<Vec3> = verts
        .iter()
        .map(|el| Vec3::new(prop_f32(el, "x"), prop_f32(el, "y"), prop_f32(el, "z")))
        .collect();

    let faces = ply.payload.get("face").map(|fs| collect_faces(fs)).unwrap_or_default();

    if !faces.is_empty() {
        // Mesh path: prefer file-supplied per-vertex normals; otherwise
        // compute smooth (area-weighted) normals from the topology.
        let has_normals = verts.first().map(|el| el.contains_key("nx")).unwrap_or(false);
        let normals = if has_normals {
            verts
                .iter()
                .map(|el| Vec3::new(prop_f32(el, "nx"), prop_f32(el, "ny"), prop_f32(el, "nz")).normalize_or_zero())
                .collect()
        } else {
            smooth_normals(&positions, &faces)
        };
        let vertices: Vec<MeshVertex> = positions
            .iter()
            .zip(normals)
            .map(|(p, n)| MeshVertex {
                position: *p,
                normal: n,
            })
            .collect();
        let indices: Vec<u32> = faces.iter().flat_map(|t| t.iter().copied()).collect();
        return Ok(LoadedModel::Mesh(MeshHandle::new(MeshData {
            vertices,
            indices: Some(indices),
        })));
    }

    // Point-cloud path: no faces. Carry per-vertex colour when present
    // (red/green/blue, 0..255), else a neutral light grey.
    let has_color = verts.first().map(|el| el.contains_key("red")).unwrap_or(false);
    let points: Vec<ScenePoint> = verts
        .iter()
        .zip(&positions)
        .map(|(el, p)| {
            let color = if has_color {
                [
                    prop_u8(el, "red") as f32 / 255.0,
                    prop_u8(el, "green") as f32 / 255.0,
                    prop_u8(el, "blue") as f32 / 255.0,
                    1.0,
                ]
            } else {
                [0.78, 0.82, 0.90, 1.0]
            };
            ScenePoint { position: *p, color }
        })
        .collect();

    Ok(LoadedModel::Points(PointsHandle::new(PointData { points })))
}

/// Area-weighted smooth vertex normals for an indexed mesh. Each triangle
/// contributes its (un-normalised, so larger faces weigh more) face normal
/// to its three vertices; a final normalise yields the per-vertex normal.
fn smooth_normals(positions: &[Vec3], faces: &[[u32; 3]]) -> Vec<Vec3> {
    let mut normals = vec![Vec3::ZERO; positions.len()];
    for tri in faces {
        let [i0, i1, i2] = *tri;
        let (a, b, c) = (positions[i0 as usize], positions[i1 as usize], positions[i2 as usize]);
        let fn_ = (b - a).cross(c - a);
        normals[i0 as usize] += fn_;
        normals[i1 as usize] += fn_;
        normals[i2 as usize] += fn_;
    }
    for n in &mut normals {
        *n = n.normalize_or_zero();
    }
    normals
}

// ---- Scene construction ----

/// Build the [`SceneSpec`] shared by the thumbnail and the lightbox, so
/// the poster matches what the user sees when they open it. No grid (a
/// model viewer wants the object, not a chart frame); auto-framing fits
/// the geometry; orbit controls for the lightbox.
fn build_scene(model: &LoadedModel) -> SceneSpec {
    let base = SceneSpec::new()
        .no_grid()
        .framing(Framing::Auto)
        .controls(CameraControls::Orbit);
    match model {
        LoadedModel::Mesh(handle) => base.mesh_with(handle.clone(), Material::glossy(Color::srgb_u8(150, 168, 198))),
        LoadedModel::Points(handle) => base.points_styled(
            handle.clone(),
            PointStyle {
                size: 3.0,
                ..Default::default()
            },
        ),
    }
}

// ---- Offscreen thumbnail rendering ----

/// Longest edge (square) of a rendered model poster, in physical px. 512
/// keeps the ~400px-tall preview rect crisp on a 2× display while bounding
/// the one-shot render + readback cost.
const THUMB_PX: u32 = 512;
const THUMB_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// Backdrop the scene composites over — a dark neutral so light models
/// read against it. The scene itself draws over this clear.
const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.05,
    g: 0.06,
    b: 0.08,
    a: 1.0,
};

/// Renders model posters offscreen. Owns a lazily-built
/// [`damascene_wgpu::Runner`] bound to [`THUMB_FORMAT`]; the host's wgpu
/// device/queue are passed in per call (the same device the live UI uses
/// — the render is a one-shot, off the main composite pass). Reused across
/// models so pipeline compilation happens once.
pub struct ModelThumbnailer {
    runner: Option<Runner>,
}

impl ModelThumbnailer {
    pub fn new() -> Self {
        Self { runner: None }
    }

    /// Render `model` to a square [`Image`] poster. Builds the runner on
    /// first use. Returns `None` only if the GPU readback mapping fails.
    pub fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, model: &LoadedModel) -> Option<Image> {
        let runner = self
            .runner
            .get_or_insert_with(|| Runner::new(device, queue, THUMB_FORMAT));
        let mut tree = chart3d(build_scene(model));
        render_scene_to_image(runner, device, queue, &mut tree, THUMB_PX, THUMB_PX)
    }
}

impl Default for ModelThumbnailer {
    fn default() -> Self {
        Self::new()
    }
}

/// Prepare + render `tree` into a fresh `w×h` target, read the pixels back,
/// and wrap them in an [`Image`]. Single-shot: blocks on the GPU via
/// `device.poll(wait)`, so callers drive it from a context where a brief
/// stall is acceptable (here, a one-time `before_paint` step per model).
fn render_scene_to_image(
    runner: &mut Runner,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tree: &mut El,
    w: u32,
    h: u32,
) -> Option<Image> {
    runner.prepare(device, queue, tree, Rect::new(0.0, 0.0, w as f32, h as f32), 1.0);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("model_thumb_target"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: THUMB_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Row pitch must respect COPY_BYTES_PER_ROW_ALIGNMENT (256); pad on the
    // GPU side and stride over the padding on readback.
    let unpadded = w * 4;
    let bytes_per_row = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("model_thumb_readback"),
        size: (bytes_per_row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("model_thumb"),
    });
    runner.render(
        device,
        &mut encoder,
        &target,
        &target_view,
        None,
        wgpu::LoadOp::Clear(CLEAR),
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    if device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
        return None;
    }
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h as usize {
        let start = row * bytes_per_row as usize;
        out.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    readback.unmap();

    Some(Image::from_rgba8(w, h, out))
}

// ---- Active model state + lightbox renderer ----

/// State for the currently-open model lightbox. Holds only the parsed
/// geometry — the orbit camera lives in damascene's `UiState`, keyed by
/// [`KEY_SCENE`], so it persists across rebuilds without us tracking it.
pub struct ActiveModel {
    pub transfer_id: String,
    pub name: String,
    pub model: LoadedModel,
}

impl ActiveModel {
    pub fn new(transfer_id: impl Into<String>, name: impl Into<String>, model: LoadedModel) -> Self {
        Self {
            transfer_id: transfer_id.into(),
            name: name.into(),
            model,
        }
    }
}

/// Build the click-to-enlarge model viewer overlay. Mirrors
/// [`crate::video::render_lightbox`]'s shape: scrim + centered panel +
/// header + body + footer hint. The body is a live `chart3d` the user can
/// orbit.
pub fn render_lightbox(active: &ActiveModel) -> El {
    let header = row([
        text(active.name.clone()).semibold().ellipsis(),
        spacer().width(Size::Fill(1.0)),
        button("Close").key(KEY_LIGHTBOX_CLOSE).ghost(),
    ])
    .gap(tokens::SPACE_2)
    .align(Align::Center)
    .width(Size::Fill(1.0));

    let scene = chart3d(build_scene(&active.model))
        .key(KEY_SCENE)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));
    let body = stack([scene])
        .clip()
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_MD)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0));

    let hint = text("Drag to orbit · Shift-drag to pan · Scroll to zoom")
        .muted()
        .font_size(tokens::TEXT_XS.size);

    let panel = column([header, body, hint])
        .style_profile(StyleProfile::Surface)
        .surface_role(SurfaceRole::Popover)
        .fill(tokens::POPOVER)
        .stroke(tokens::BORDER)
        .stroke_width(1.0)
        .radius(tokens::RADIUS_LG)
        .padding(Sides::all(tokens::SPACE_4))
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
        .block_pointer();

    let centered = stack([panel]).fill_size().padding(Sides::all(tokens::SPACE_7));

    overlay([scrim(KEY_LIGHTBOX_DISMISS), centered])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_model_name_matches_stl_and_ply() {
        assert!(is_model_name("part.stl"));
        assert!(is_model_name("Part.STL"));
        assert!(is_model_name("scan.ply"));
        assert!(!is_model_name("photo.png"));
        assert!(!is_model_name("clip.mp4"));
        assert!(!is_model_name("noext"));
    }

    #[test]
    fn open_model_key_round_trips() {
        let k = open_model_key("abc-123");
        assert_eq!(parse_open_model_key(&k), Some("abc-123"));
        assert_eq!(parse_open_model_key("model:open:"), Some(""));
        assert_eq!(parse_open_model_key("other:xyz"), None);
    }

    #[test]
    fn smooth_normals_point_outward_for_a_tetra_face() {
        // A single triangle in the z=0 plane wound CCW → normal +Z.
        let positions = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
        ];
        let normals = smooth_normals(&positions, &[[0, 1, 2]]);
        for n in normals {
            assert!((n - Vec3::Z).length() < 1e-5, "expected +Z, got {n:?}");
        }
    }
}
