//! Excalidraw-like whiteboard as a workspace item. Reads and writes `.excalidraw` files.

use std::{
    borrow::Cow,
    cell::Cell,
    collections::{HashMap, HashSet},
    f32::consts::PI,
    io::Cursor,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gpui::{
    App, Bounds, ClipboardEntry, Context, Corners, Entity, EntityId, EventEmitter, ExternalPaths,
    FocusHandle, Focusable, Hsla, Image, ImageFormat, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Render, Rgba, ScrollWheelEvent,
    SharedString, Task, TextAlign, TextRun, Window, actions, canvas, div, fill, font, point,
    prelude::*, px, rgb, size,
};
use project::{Project, ProjectEntryId, ProjectPath};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use workspace::{
    Pane, Workspace,
    item::{Item, ItemBufferKind, ItemEvent, SaveOptions},
};

actions!(
    whiteboard,
    [
        /// Opens a new, unsaved whiteboard.
        OpenWhiteboard
    ]
);

const FONT: &str = "Excalifont";
/// Excalidraw's id for Excalifont.
const FONT_FAMILY_ID: u32 = 5;
const LINE_HEIGHT: f32 = 1.25;
const STROKES: [&str; 5] = ["#1e1e1e", "#e03131", "#2f9e44", "#1971c2", "#f08c00"];
const FILLS: [&str; 5] = ["transparent", "#ffc9c9", "#b2f2bb", "#a5d8ff", "#ffec99"];
const STICKY: &str = "#ffec99";

pub fn init(cx: &mut App) {
    let font_bytes: &'static [u8] = include_bytes!("../fonts/Excalifont-Regular.ttf");
    if let Err(e) = cx.text_system().add_fonts(vec![Cow::Borrowed(font_bytes)]) {
        log::error!("whiteboard: failed to load Excalifont: {e}");
    }
    workspace::register_project_item::<Whiteboard>(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenWhiteboard, window, cx| {
            let board = cx.new(|cx| Whiteboard::new(None, Doc::default(), cx));
            workspace.add_item_to_active_pane(Box::new(board), None, true, window, cx);
        });
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Data model (subset of packages/element/src/types.ts; unknown fields kept in `rest`).

#[derive(Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    elements: Vec<Element>,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Element {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    x: f32,
    y: f32,
    #[serde(default)]
    width: f32,
    #[serde(default)]
    height: f32,
    #[serde(default)]
    angle: f32,
    #[serde(default = "default_stroke")]
    stroke_color: String,
    #[serde(default = "default_bg")]
    background_color: String,
    #[serde(default = "default_fill")]
    fill_style: String,
    #[serde(default = "default_stroke_width")]
    stroke_width: f32,
    #[serde(default = "default_roughness")]
    roughness: f32,
    #[serde(default = "default_opacity")]
    opacity: f32,
    #[serde(default)]
    seed: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    points: Vec<V>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    font_size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    font_family: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    container_id: Option<String>,
    #[serde(default)]
    is_deleted: bool,
    #[serde(flatten)]
    rest: Map<String, Value>,
}

fn default_stroke() -> String {
    STROKES[0].into()
}
fn default_bg() -> String {
    "transparent".into()
}
fn default_fill() -> String {
    "solid".into()
}
fn default_stroke_width() -> f32 {
    2.
}
fn default_roughness() -> f32 {
    1.
}
fn default_opacity() -> f32 {
    100.
}

type V = [f32; 2];

impl Element {
    fn new(kind: &str, p: V, stroke: &str, bg: &str) -> Self {
        let mut rest = Map::new();
        match kind {
            "freedraw" => {
                rest.insert("pressures".into(), json!([]));
                rest.insert("simulatePressure".into(), json!(true));
            }
            "arrow" => {
                rest.insert("startArrowhead".into(), Value::Null);
                rest.insert("endArrowhead".into(), json!("arrow"));
            }
            _ => {}
        }
        Self {
            id: rand_id(),
            kind: kind.into(),
            x: p[0],
            y: p[1],
            width: 0.,
            height: 0.,
            angle: 0.,
            stroke_color: stroke.into(),
            background_color: bg.into(),
            fill_style: "solid".into(),
            stroke_width: 2.,
            roughness: 1.,
            opacity: 100.,
            seed: rand_u32() >> 1,
            points: Vec::new(),
            text: None,
            font_size: None,
            font_family: None,
            container_id: None,
            is_deleted: false,
            rest,
        }
    }

    fn is_linear(&self) -> bool {
        matches!(self.kind.as_str(), "line" | "arrow" | "freedraw")
    }

    fn abs_points(&self) -> Vec<V> {
        self.points
            .iter()
            .map(|p| [self.x + p[0], self.y + p[1]])
            .collect()
    }

    /// (min, max) in scene coordinates, ignoring rotation.
    fn bbox(&self) -> (V, V) {
        if self.is_linear() && !self.points.is_empty() {
            let pts = self.abs_points();
            let mut lo = pts[0];
            let mut hi = pts[0];
            for p in &pts {
                lo = [lo[0].min(p[0]), lo[1].min(p[1])];
                hi = [hi[0].max(p[0]), hi[1].max(p[1])];
            }
            (lo, hi)
        } else {
            (
                [self.x, self.y],
                [self.x + self.width, self.y + self.height],
            )
        }
    }

    fn center(&self) -> V {
        let (lo, hi) = self.bbox();
        [(lo[0] + hi[0]) / 2., (lo[1] + hi[1]) / 2.]
    }

    fn fit_points(&mut self) {
        let (lo, hi) = self.bbox();
        self.width = hi[0] - lo[0];
        self.height = hi[1] - lo[1];
    }

    fn hit(&self, p: V) -> bool {
        let p = rotate(p, self.center(), -self.angle);
        let tol = 6. + self.stroke_width;
        if self.is_linear() {
            let pts = self.abs_points();
            if pts.len() == 1 {
                return dist(p, pts[0]) < tol;
            }
            return pts.windows(2).any(|w| seg_dist(p, w[0], w[1]) < tol);
        }
        let (lo, hi) = self.bbox();
        p[0] >= lo[0] - tol && p[0] <= hi[0] + tol && p[1] >= lo[1] - tol && p[1] <= hi[1] + tol
    }

    /// Bounding-box corners in scene coordinates, rotation applied.
    fn corners(&self) -> [V; 4] {
        let (lo, hi) = self.bbox();
        let c = self.center();
        [lo, [hi[0], lo[1]], hi, [lo[0], hi[1]]].map(|p| rotate(p, c, self.angle))
    }

    fn file_id(&self) -> Option<&str> {
        self.rest.get("fileId").and_then(Value::as_str)
    }
}

// ---------------------------------------------------------------------------
// Images: `files` entries ({mimeType, id, dataURL, created}) are decoded once; the
// dataURL string is dropped from memory and rebuilt from the bytes on save.

struct ImageFile {
    meta: Map<String, Value>,
    image: Option<Arc<Image>>,
}

/// Bytes -> renderable image + pixel size, format sniffed from the content.
fn decode_image(bytes: Vec<u8>) -> Option<(Arc<Image>, V)> {
    let fmt = image::guess_format(&bytes).ok()?;
    let gpui_fmt = ImageFormat::from_mime_type(fmt.to_mime_type())?;
    let (w, h) = image::ImageReader::with_format(Cursor::new(&bytes), fmt)
        .into_dimensions()
        .ok()?;
    Some((
        Arc::new(Image::from_bytes(gpui_fmt, bytes)),
        [w as f32, h as f32],
    ))
}

impl ImageFile {
    fn from_value(v: Value) -> Self {
        let Value::Object(mut meta) = v else {
            return Self {
                meta: Map::new(),
                image: None,
            };
        };
        let image = meta
            .get("dataURL")
            .and_then(Value::as_str)
            .and_then(|u| u.split_once("base64,"))
            .and_then(|(_, b)| STANDARD.decode(b).ok())
            .and_then(decode_image)
            .map(|(img, _)| img);
        // Undecodable entries keep their dataURL so saving never loses them.
        if image.is_some() {
            meta.remove("dataURL");
        }
        Self { meta, image }
    }

    fn to_value(&self) -> Value {
        let mut m = self.meta.clone();
        if let Some(img) = &self.image {
            let url = format!(
                "data:{};base64,{}",
                img.format.mime_type(),
                STANDARD.encode(&img.bytes)
            );
            m.insert("dataURL".into(), json!(url));
        }
        Value::Object(m)
    }
}

// ---------------------------------------------------------------------------
// Selection handles: 8 resize handles (unit offsets from the box center) + rotate.

const HANDLES: [V; 9] = [
    [-1., -1.],
    [0., -1.],
    [1., -1.],
    [1., 0.],
    [1., 1.],
    [0., 1.],
    [-1., 1.],
    [-1., 0.],
    [0., -1.],
];
const ROTATE_HANDLE: usize = 8;

fn handle_pos(lo: V, hi: V, angle: f32, zoom: f32, i: usize) -> V {
    let pad = 6. / zoom;
    let c = [(lo[0] + hi[0]) / 2., (lo[1] + hi[1]) / 2.];
    let half = [(hi[0] - lo[0]) / 2. + pad, (hi[1] - lo[1]) / 2. + pad];
    let extra = if i == ROTATE_HANDLE { 20. / zoom } else { 0. };
    let [sx, sy] = HANDLES[i];
    rotate([c[0] + sx * half[0], c[1] + sy * half[1] - extra], c, angle)
}

// ---------------------------------------------------------------------------
// Math + randomness.

fn dist(a: V, b: V) -> f32 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}
fn lerp(a: V, b: V, t: f32) -> V {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}
fn unit(v: V) -> V {
    let l = v[0].hypot(v[1]);
    if l == 0. {
        [0., 0.]
    } else {
        [v[0] / l, v[1] / l]
    }
}
fn per(v: V) -> V {
    [v[1], -v[0]]
}
fn rotate(p: V, c: V, a: f32) -> V {
    if a == 0. {
        return p;
    }
    let (s, co) = a.sin_cos();
    let (dx, dy) = (p[0] - c[0], p[1] - c[1]);
    [c[0] + dx * co - dy * s, c[1] + dx * s + dy * co]
}
fn seg_dist(p: V, a: V, b: V) -> f32 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len2 == 0. {
        0.
    } else {
        (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / len2).clamp(0., 1.)
    };
    dist(p, lerp(a, b, t))
}

fn rand_u32() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0x2545_f491);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut x = nanos ^ COUNTER.fetch_add(0x9e37_79b9, Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}
fn rand_id() -> String {
    format!("{:08x}{:08x}", rand_u32(), rand_u32())
}

/// rough.js `Random`: Park-Miller LCG seeded by the element seed, so jitter is stable.
struct Rough(i32);
impl Rough {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(48271);
        (self.0 & 0x7fff_ffff) as f32 / 2_147_483_648.
    }
    fn off(&mut self, r: f32) -> f32 {
        (self.next() * 2. - 1.) * r
    }
}

// ---------------------------------------------------------------------------
// perfect-freehand getStroke (variable-width freedraw outline).
// Excalidraw options: size = strokeWidth * 4.25, thinning 0.6, smoothing 0.5,
// streamline 0.5, simulated pressure, easeOutSine, last = true.
// ponytail: skips sharp-corner caps and tapering; add if strokes with hairpin turns look off.

fn freehand(input: &[V], size: f32) -> Vec<V> {
    let (thinning, smoothing, streamline) = (0.6, 0.5, 0.5);
    let ease = |t: f32| (t * PI / 2.).sin();
    let radius_for = |pressure: f32| size * ease(0.5 - thinning * (0.5 - pressure));
    let simulate = |prev: f32, d: f32| {
        let sp = (d / size).min(1.);
        (prev + ((1. - sp) - prev) * (sp * 0.275)).min(1.)
    };
    let mut input = input.to_vec();
    match input.len() {
        0 => return Vec::new(),
        1 => input.push([input[0][0] + 1., input[0][1] + 1.]),
        _ => {}
    }

    // getStrokePoints: streamlined points with distance and backward unit vector.
    let t = 0.15 + (1. - streamline) * 0.85;
    let mut pts: Vec<(V, f32, V)> = vec![(input[0], 0., [1., 1.])];
    for (i, &raw) in input.iter().enumerate().skip(1) {
        let prev = pts[pts.len() - 1].0;
        let p = if i == input.len() - 1 {
            raw
        } else {
            lerp(prev, raw, t)
        };
        let d = dist(prev, p);
        if d == 0. {
            continue;
        }
        pts.push((p, d, unit([prev[0] - p[0], prev[1] - p[1]])));
    }
    if pts.len() < 2 {
        pts.push((
            [input[0][0] + 1., input[0][1] + 1.],
            1.414,
            unit([-1., -1.]),
        ));
    }
    pts[0].2 = pts[1].2;

    // getStrokeOutlinePoints.
    let min_dist = (size * smoothing).powi(2);
    let mut pressure = pts
        .iter()
        .take(10)
        .fold(0.25, |acc, p| (acc + simulate(acc, p.1)) / 2.);
    let (mut left, mut right) = (Vec::<V>::new(), Vec::<V>::new());
    let mut radii = Vec::with_capacity(pts.len());
    for (i, &(p, d, v)) in pts.iter().enumerate() {
        pressure = simulate(pressure, d);
        let radius = radius_for(pressure).max(0.01);
        radii.push(radius);
        let next_v = pts.get(i + 1).map_or(v, |n| n.2);
        let dpr = v[0] * next_v[0] + v[1] * next_v[1];
        let o = per(lerp(next_v, v, dpr));
        let o = [o[0] * radius, o[1] * radius];
        let l = [p[0] - o[0], p[1] - o[1]];
        let r = [p[0] + o[0], p[1] + o[1]];
        let far =
            |side: &Vec<V>, q: V| side.last().is_none_or(|s| (dist(*s, q)).powi(2) > min_dist);
        if i <= 1 || i == pts.len() - 1 || far(&left, l) {
            left.push(l);
        }
        if i <= 1 || i == pts.len() - 1 || far(&right, r) {
            right.push(r);
        }
    }

    // Round caps: half circles around the first and last point.
    let cap = |c: V, from: V, bulge: V, out: &mut Vec<V>| {
        let a = [from[0] - c[0], from[1] - c[1]];
        for k in 1..13 {
            let th = PI * k as f32 / 13.;
            let (s, co) = th.sin_cos();
            out.push([
                c[0] + a[0] * co + bulge[0] * s,
                c[1] + a[1] * co + bulge[1] * s,
            ]);
        }
    };
    let n = pts.len() - 1;
    let mut outline = left.clone();
    let (end, end_v, end_r) = (pts[n].0, pts[n].2, radii[n]);
    cap(
        end,
        left[left.len() - 1],
        [-end_v[0] * end_r, -end_v[1] * end_r],
        &mut outline,
    );
    outline.extend(right.iter().rev());
    let (start, start_v, start_r) = (pts[0].0, pts[0].2, radii[0]);
    cap(
        start,
        right[0],
        [start_v[0] * start_r, start_v[1] * start_r],
        &mut outline,
    );
    outline
}

// ---------------------------------------------------------------------------
// Rendering helpers.

/// Excalidraw dark mode is `filter: invert(93%) hue-rotate(180deg)`; files keep light colors.
fn color(hex: &str, opacity: f32) -> Option<Hsla> {
    if hex == "transparent" {
        return None;
    }
    let c = Rgba::try_from(hex).ok()?;
    let inv = |v: f32| v + 0.93 * (1. - 2. * v);
    let mut h: Hsla = Rgba {
        r: inv(c.r),
        g: inv(c.g),
        b: inv(c.b),
        a: c.a,
    }
    .into();
    h.h = (h.h + 0.5).fract();
    h.a *= opacity / 100.;
    Some(h)
}

/// Closed outline for shapes, open polyline for line/arrow.
fn outline(el: &Element) -> (Vec<V>, bool) {
    let (x, y, w, h) = (el.x, el.y, el.width, el.height);
    match el.kind.as_str() {
        "rectangle" => (vec![[x, y], [x + w, y], [x + w, y + h], [x, y + h]], true),
        "diamond" => (
            vec![
                [x + w / 2., y],
                [x + w, y + h / 2.],
                [x + w / 2., y + h],
                [x, y + h / 2.],
            ],
            true,
        ),
        "ellipse" => (
            (0..36)
                .map(|i| {
                    let a = i as f32 / 36. * 2. * PI;
                    [x + w / 2. * (1. + a.cos()), y + h / 2. * (1. + a.sin())]
                })
                .collect(),
            true,
        ),
        _ => (el.abs_points(), false),
    }
}

struct Painter<'a> {
    /// Screen position of scene (0, 0).
    origin: Point<Pixels>,
    zoom: f32,
    images: &'a HashMap<String, Arc<Image>>,
    window: &'a mut Window,
    cx: &'a mut App,
}

impl Painter<'_> {
    fn pt(&self, p: V) -> Point<Pixels> {
        self.origin + point(px(p[0] * self.zoom), px(p[1] * self.zoom))
    }

    /// Dashed selection outline, padded by a constant screen distance.
    fn dashed_box(&mut self, lo: V, hi: V, angle: f32) {
        let pad = 6. / self.zoom;
        let (lo, hi) = ([lo[0] - pad, lo[1] - pad], [hi[0] + pad, hi[1] + pad]);
        let c = [(lo[0] + hi[0]) / 2., (lo[1] + hi[1]) / 2.];
        let corners =
            [lo, [hi[0], lo[1]], hi, [lo[0], hi[1]], lo].map(|p| self.pt(rotate(p, c, angle)));
        let mut b = PathBuilder::stroke(px(1.)).dash_array(&[px(4.), px(3.)]);
        b.move_to(corners[0]);
        for p in &corners[1..] {
            b.line_to(*p);
        }
        if let Ok(path) = b.build() {
            self.window.paint_path(path, rgb(0xa8a5ff));
        }
    }

    fn fill_poly(&mut self, pts: &[V], color: Hsla) {
        if pts.len() < 3 {
            return;
        }
        let mut b = PathBuilder::fill();
        b.add_polygon(&pts.iter().map(|p| self.pt(*p)).collect::<Vec<_>>(), true);
        if let Ok(path) = b.build() {
            self.window.paint_path(path, color);
        }
    }

    /// Simplified rough.js: two passes, each edge a slightly bowed quadratic with jittered ends.
    fn rough_stroke(&mut self, el: &Element, pts: &[V], closed: bool, smooth: bool, color: Hsla) {
        if pts.len() < 2 {
            return;
        }
        let mut rng = Rough(el.seed.max(1) as i32);
        let passes = if el.roughness > 0. { 2 } else { 1 };
        for _ in 0..passes {
            let mut b = PathBuilder::stroke(px(el.stroke_width.max(0.5) * self.zoom));
            if smooth {
                // Ellipse: jitter each vertex, single closed curve.
                let r = el.roughness * 1.2;
                let j: Vec<V> = pts
                    .iter()
                    .map(|p| [p[0] + rng.off(r), p[1] + rng.off(r)])
                    .collect();
                b.move_to(self.pt(j[0]));
                for p in j.iter().skip(1).chain(std::iter::once(&j[0])) {
                    b.line_to(self.pt(*p));
                }
            } else {
                let n = if closed { pts.len() } else { pts.len() - 1 };
                for i in 0..n {
                    let (a, c) = (pts[i], pts[(i + 1) % pts.len()]);
                    let len = dist(a, c);
                    let r = (el.roughness * 2.).min(len / 10.);
                    let a = [a[0] + rng.off(r), a[1] + rng.off(r)];
                    let c = [c[0] + rng.off(r), c[1] + rng.off(r)];
                    let n = per(unit([c[0] - a[0], c[1] - a[1]]));
                    let bow = rng.off(el.roughness * len * 0.02);
                    let mid = lerp(a, c, 0.5 + rng.off(0.1));
                    b.move_to(self.pt(a));
                    b.curve_to(
                        self.pt(c),
                        self.pt([mid[0] + n[0] * bow, mid[1] + n[1] * bow]),
                    );
                }
            }
            if let Ok(path) = b.build() {
                self.window.paint_path(path, color);
            }
        }
    }

    fn text(&mut self, el: &Element, text: &str, container: Option<&Element>) {
        let Some(color) = color(&el.stroke_color, el.opacity) else {
            return;
        };
        let size = el.font_size.unwrap_or(20.);
        let line_h = size * LINE_HEIGHT;
        let lines: Vec<&str> = text.split('\n').collect();
        let mut y = el.y;
        let center = container.map(|c| {
            let ctr = c.center();
            y = ctr[1] - lines.len() as f32 * line_h / 2.;
            ctr[0]
        });
        for line in lines {
            let run = TextRun {
                len: line.len(),
                font: font(FONT),
                color,
                ..Default::default()
            };
            let shaped = self.window.text_system().shape_line(
                SharedString::from(line.to_string()),
                px(size * self.zoom),
                &[run],
                None,
            );
            let x = center.map_or(el.x, |cx| cx - f32::from(shaped.width) / self.zoom / 2.);
            let origin = self.pt([x, y]);
            shaped
                .paint(
                    origin,
                    px(line_h * self.zoom),
                    TextAlign::Left,
                    None,
                    self.window,
                    self.cx,
                )
                .ok();
            y += line_h;
        }
    }

    fn element(&mut self, el: &Element, all: &[Element], editing: Option<&str>) {
        let stroke = color(&el.stroke_color, el.opacity);
        let rot = |pts: Vec<V>| -> Vec<V> {
            let c = el.center();
            pts.into_iter().map(|p| rotate(p, c, el.angle)).collect()
        };
        match el.kind.as_str() {
            "text" => {
                let mut text = el.text.clone().unwrap_or_default();
                if editing == Some(el.id.as_str()) {
                    text.push('|');
                }
                let container = el
                    .container_id
                    .as_ref()
                    .and_then(|id| all.iter().find(|c| &c.id == id && !c.is_deleted));
                self.text(el, &text, container);
            }
            "freedraw" => {
                if let Some(c) = stroke {
                    let pts = rot(freehand(&el.abs_points(), el.stroke_width * 4.25));
                    self.fill_poly(&pts, c);
                }
            }
            "rectangle" | "diamond" | "ellipse" | "line" | "arrow" => {
                let (pts, closed) = outline(el);
                let pts = rot(pts);
                if closed && let Some(bg) = color(&el.background_color, el.opacity) {
                    // ponytail: hachure/cross-hatch drawn as translucent solid fill.
                    let bg = if el.fill_style == "solid" {
                        bg
                    } else {
                        bg.opacity(0.5)
                    };
                    self.fill_poly(&pts, bg);
                }
                let Some(c) = stroke else { return };
                self.rough_stroke(el, &pts, closed, el.kind == "ellipse", c);
                let has_head =
                    el.kind == "arrow" && el.rest.get("endArrowhead").is_none_or(|v| !v.is_null());
                if has_head && pts.len() >= 2 {
                    let (tip, from) = (pts[pts.len() - 1], pts[pts.len() - 2]);
                    let len = (dist(tip, from) * 0.5).min(20.);
                    let back = unit([from[0] - tip[0], from[1] - tip[1]]);
                    let wing = |a: f32| {
                        let v = rotate(back, [0., 0.], a);
                        [tip[0] + v[0] * len, tip[1] + v[1] * len]
                    };
                    let head = [wing(0.45), tip, wing(-0.45)];
                    self.rough_stroke(el, &head, false, false, c);
                }
            }
            "image" => {
                // ponytail: images ignore `angle` (paint_image can't rotate); scale/crop ignored too.
                let b = Bounds::new(
                    self.pt([el.x, el.y]),
                    size(px(el.width * self.zoom), px(el.height * self.zoom)),
                );
                let img = el.file_id().and_then(|id| self.images.get(id)).cloned();
                match img.and_then(|i| i.use_render_image(self.window, self.cx)) {
                    Some(data) => {
                        self.window
                            .paint_image(b, b, Corners::default(), data, 0, false)
                            .ok();
                    }
                    None => self
                        .window
                        .paint_quad(fill(b, gpui::hsla(0., 0., 1., 0.06))),
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// File as a project item.

pub struct BoardFile {
    path: ProjectPath,
    abs_path: PathBuf,
    entry_id: Option<ProjectEntryId>,
    doc: Option<Doc>,
}

fn read_doc(path: &std::path::Path) -> Result<Doc> {
    let doc: Doc = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if doc.rest.get("type").and_then(Value::as_str) != Some("excalidraw") {
        return Err(anyhow!("not an excalidraw file"));
    }
    Ok(doc)
}

impl project::ProjectItem for BoardFile {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if path.path.extension() != Some("excalidraw") {
            return None;
        }
        let (abs_path, entry_id) = {
            let project = project.read(cx);
            (
                project.absolute_path(path, cx)?,
                project.entry_for_path(path, cx).map(|e| e.id),
            )
        };
        // ponytail: sync read of a local file; remote projects need project.open_buffer instead.
        let doc = read_doc(&abs_path);
        let path = path.clone();
        Some(Task::ready(doc.map(|doc| {
            cx.new(|_| BoardFile {
                path,
                abs_path,
                entry_id,
                doc: Some(doc),
            })
        })))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.path.clone())
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// The view.

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Select,
    Rectangle,
    Diamond,
    Ellipse,
    Arrow,
    Line,
    Draw,
    Text,
    Eraser,
}

const TOOLS: [(Tool, &str, &str); 9] = [
    (Tool::Select, "Select", "v"),
    (Tool::Rectangle, "Rect", "r"),
    (Tool::Diamond, "Diamond", "d"),
    (Tool::Ellipse, "Ellipse", "o"),
    (Tool::Arrow, "Arrow", "a"),
    (Tool::Line, "Line", "l"),
    (Tool::Draw, "Draw", "p"),
    (Tool::Text, "Text", "t"),
    (Tool::Eraser, "Eraser", "e"),
];

enum Drag {
    None,
    Move {
        last: V,
        moved: bool,
    },
    Create {
        start: V,
    },
    Erase,
    Marquee {
        start: V,
        end: V,
    },
    /// Resize (handles 0..8) or rotate (ROTATE_HANDLE) the selection; the pre-drag
    /// elements are the last undo snapshot.
    Transform {
        handle: usize,
        lo: V,
        hi: V,
        angle: f32,
        start: V,
        moved: bool,
    },
}

pub struct Whiteboard {
    focus: FocusHandle,
    file: Option<Entity<BoardFile>>,
    elements: Vec<Element>,
    rest: Map<String, Value>,
    files: HashMap<String, ImageFile>,
    undo: Vec<Vec<Element>>,
    redo: Vec<Vec<Element>>,
    tool: Tool,
    stroke: usize,
    fill: usize,
    selected: Vec<String>,
    editing: Option<String>,
    drag: Drag,
    dirty: bool,
    /// Excalidraw convention: screen = (scene + scroll) * zoom.
    scroll: V,
    zoom: f32,
    cursor: Option<Point<Pixels>>,
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl Whiteboard {
    fn new(file: Option<Entity<BoardFile>>, doc: Doc, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            focus: cx.focus_handle(),
            file,
            elements: Vec::new(),
            rest: Map::new(),
            files: HashMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: Tool::Select,
            stroke: 0,
            fill: 0,
            selected: Vec::new(),
            editing: None,
            drag: Drag::None,
            dirty: false,
            scroll: [0., 0.],
            zoom: 1.,
            cursor: None,
            bounds: Rc::default(),
        };
        this.set_doc(doc);
        this
    }

    fn set_doc(&mut self, mut doc: Doc) {
        self.elements = doc.elements.into_iter().filter(|e| !e.is_deleted).collect();
        let used: HashSet<&str> = self.elements.iter().filter_map(Element::file_id).collect();
        let files = match doc.rest.remove("files") {
            Some(Value::Object(m)) => m
                .into_iter()
                .filter(|(id, _)| used.contains(id.as_str()))
                .map(|(id, v)| (id, ImageFile::from_value(v)))
                .collect(),
            _ => HashMap::new(),
        };
        self.files = files;
        let app = doc.rest.get("appState");
        let num = |ptr: &str| app.and_then(|a| a.pointer(ptr)).and_then(Value::as_f64);
        self.zoom = num("/zoom/value").map_or(1., |z| (z as f32).clamp(0.1, 30.));
        self.scroll = [
            num("/scrollX").unwrap_or(0.) as f32,
            num("/scrollY").unwrap_or(0.) as f32,
        ];
        self.rest = doc.rest;
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.elements.iter().position(|e| e.id == id)
    }

    fn bound_text(&self, container: &str) -> Option<usize> {
        self.elements
            .iter()
            .position(|e| e.container_id.as_deref() == Some(container))
    }

    /// Selected, or bound text of a selected container.
    fn is_selected(&self, el: &Element) -> bool {
        self.selected.contains(&el.id)
            || el
                .container_id
                .as_ref()
                .is_some_and(|c| self.selected.contains(c))
    }

    fn scene(&self, pos: Point<Pixels>) -> V {
        let o = self.bounds.get().origin;
        [
            f32::from(pos.x - o.x) / self.zoom - self.scroll[0],
            f32::from(pos.y - o.y) / self.zoom - self.scroll[1],
        ]
    }

    /// Topmost element under `p`; bound text resolves to its container.
    fn hit(&self, p: V) -> Option<usize> {
        let i = self.elements.iter().rposition(|e| e.hit(p))?;
        match &self.elements[i].container_id {
            Some(c) => self.index(c).or(Some(i)),
            None => Some(i),
        }
    }

    /// (lo, hi, angle) of the selection; a single element keeps its own rotation.
    fn selection_box(&self) -> Option<(V, V, f32)> {
        let sel: Vec<&Element> = self
            .elements
            .iter()
            .filter(|e| self.selected.contains(&e.id))
            .collect();
        match sel.as_slice() {
            [] => None,
            [e] => {
                let (lo, hi) = e.bbox();
                Some((lo, hi, e.angle))
            }
            many => {
                let pts: Vec<V> = many.iter().flat_map(|e| e.corners()).collect();
                let lo = pts
                    .iter()
                    .fold(pts[0], |a, p| [a[0].min(p[0]), a[1].min(p[1])]);
                let hi = pts
                    .iter()
                    .fold(pts[0], |a, p| [a[0].max(p[0]), a[1].max(p[1])]);
                Some((lo, hi, 0.))
            }
        }
    }

    /// Re-centers bound text on its (moved/resized/rotated) selected container.
    fn sync_bound_text(&mut self) {
        let boxes: Vec<(String, V, f32)> = self
            .elements
            .iter()
            .filter(|e| self.selected.contains(&e.id))
            .map(|e| (e.id.clone(), e.center(), e.angle))
            .collect();
        for t in &mut self.elements {
            let Some((_, c, a)) = t
                .container_id
                .as_ref()
                .and_then(|id| boxes.iter().find(|b| &b.0 == id))
            else {
                continue;
            };
            t.x = c[0] - t.width / 2.;
            t.y = c[1] - t.height / 2.;
            t.angle = *a;
        }
    }

    fn zoom_to(&mut self, zoom: f32, at: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let b = self.bounds.get();
        let at = at.unwrap_or(b.center());
        let p = self.scene(at);
        self.zoom = zoom.clamp(0.1, 30.);
        let s = [f32::from(at.x - b.origin.x), f32::from(at.y - b.origin.y)];
        self.scroll = [s[0] / self.zoom - p[0], s[1] / self.zoom - p[1]];
        cx.notify();
    }

    /// Scene point under the cursor, or the viewport center.
    fn anchor(&self) -> V {
        let b = self.bounds.get();
        self.scene(self.cursor.filter(|c| b.contains(c)).unwrap_or(b.center()))
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.elements.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        if !self.dirty {
            self.dirty = true;
            cx.emit(ItemEvent::UpdateTab);
        }
        cx.notify();
    }

    fn history(&mut self, redo: bool, cx: &mut Context<Self>) {
        let (from, to) = if redo {
            (&mut self.redo, &mut self.undo)
        } else {
            (&mut self.undo, &mut self.redo)
        };
        if let Some(snapshot) = from.pop() {
            to.push(std::mem::replace(&mut self.elements, snapshot));
            self.selected.clear();
            self.editing = None;
            self.changed(cx);
        }
    }

    fn remove(&mut self, id: &str) {
        self.elements
            .retain(|e| e.id != id && e.container_id.as_deref() != Some(id));
    }

    fn add_image(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) -> bool {
        let Some((image, [w, h])) = decode_image(bytes) else {
            return false;
        };
        let id = format!("{:016x}", image.id());
        let s = (600. / w.max(h)).min(1.);
        let (w, h) = (w * s, h * s);
        let p = self.anchor();
        let mut el = Element::new(
            "image",
            [p[0] - w / 2., p[1] - h / 2.],
            "transparent",
            "transparent",
        );
        el.width = w;
        el.height = h;
        el.rest.insert("fileId".into(), json!(id));
        el.rest.insert("status".into(), json!("saved"));
        el.rest.insert("scale".into(), json!([1, 1]));
        el.rest.insert("crop".into(), Value::Null);
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.files.entry(id.clone()).or_insert_with(|| {
            let mut meta = Map::new();
            meta.insert("mimeType".into(), json!(image.format.mime_type()));
            meta.insert("id".into(), json!(id));
            meta.insert("created".into(), json!(created));
            ImageFile {
                meta,
                image: Some(image),
            }
        });
        self.checkpoint();
        self.selected = vec![el.id.clone()];
        self.elements.push(el);
        self.tool = Tool::Select;
        self.changed(cx);
        true
    }

    fn add_image_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> bool {
        let mut any = false;
        for path in paths {
            if let Ok(bytes) = std::fs::read(path) {
                any |= self.add_image(bytes, cx);
            }
        }
        any
    }

    fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        // A bitmap and a file path are usually the same image; prefer the bitmap.
        let bitmap = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(img) => Some(img.bytes.clone()),
            _ => None,
        });
        if let Some(bytes) = bitmap {
            return self.add_image(bytes, cx);
        }
        let paths: Vec<PathBuf> = item
            .entries()
            .iter()
            .filter_map(|e| match e {
                ClipboardEntry::ExternalPaths(p) => Some(p.paths().to_vec()),
                _ => None,
            })
            .flatten()
            .collect();
        self.add_image_files(&paths, cx)
    }

    fn duplicate(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        self.checkpoint();
        let picked: Vec<Element> = self
            .elements
            .iter()
            .filter(|e| self.is_selected(e))
            .cloned()
            .collect();
        let ids: HashMap<String, String> =
            picked.iter().map(|e| (e.id.clone(), rand_id())).collect();
        for mut e in picked {
            e.id = ids[&e.id].clone();
            e.seed = rand_u32() >> 1;
            e.x += 10.;
            e.y += 10.;
            e.container_id = e.container_id.map(|c| ids.get(&c).cloned().unwrap_or(c));
            if let Some(Value::Array(list)) = e.rest.get_mut("boundElements") {
                // ponytail: drops bindings to arrows that weren't duplicated along.
                list.retain(|b| {
                    b.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|i| ids.contains_key(i))
                });
                for b in list.iter_mut() {
                    let new = b
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|i| ids.get(i))
                        .cloned();
                    if let Some(new) = new {
                        b["id"] = json!(new);
                    }
                }
            }
            self.elements.push(e);
        }
        self.selected = self
            .selected
            .iter()
            .filter_map(|id| ids.get(id).cloned())
            .collect();
        self.changed(cx);
    }

    fn start_text(&mut self, p: V, container: Option<String>) {
        if let Some(i) = container.as_deref().and_then(|c| self.bound_text(c)) {
            self.checkpoint();
            self.editing = Some(self.elements[i].id.clone());
            return;
        }
        self.checkpoint();
        let mut el = Element::new("text", p, STROKES[self.stroke], "transparent");
        el.text = Some(String::new());
        el.font_size = Some(20.);
        el.font_family = Some(FONT_FAMILY_ID);
        let (h, v) = if container.is_some() {
            ("center", "middle")
        } else {
            ("left", "top")
        };
        el.rest.insert("textAlign".into(), json!(h));
        el.rest.insert("verticalAlign".into(), json!(v));
        el.rest.insert("lineHeight".into(), json!(LINE_HEIGHT));
        el.rest.insert("autoResize".into(), json!(true));
        if let Some(ci) = container.as_deref().and_then(|c| self.index(c)) {
            let bound = self.elements[ci]
                .rest
                .entry("boundElements")
                .or_insert(json!([]));
            if !bound.is_array() {
                *bound = json!([]);
            }
            if let Some(list) = bound.as_array_mut() {
                list.push(json!({ "type": "text", "id": el.id }));
            }
        }
        el.container_id = container;
        self.selected = vec![el.container_id.clone().unwrap_or(el.id.clone())];
        self.editing = Some(el.id.clone());
        self.elements.push(el);
    }

    /// Ends text editing: drops empty text, measures the rest, centers bound text.
    fn commit_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.editing.take().and_then(|id| self.index(&id)) else {
            return;
        };
        let text = self.elements[i].text.clone().unwrap_or_default();
        if text.trim().is_empty() {
            let id = self.elements[i].id.clone();
            self.remove(&id);
            self.changed(cx);
            return;
        }
        let size = self.elements[i].font_size.unwrap_or(20.);
        let width = text
            .split('\n')
            .map(|line| {
                let run = TextRun {
                    len: line.len(),
                    font: font(FONT),
                    ..Default::default()
                };
                let shaped = window.text_system().shape_line(
                    line.to_string().into(),
                    px(size),
                    &[run],
                    None,
                );
                f32::from(shaped.width)
            })
            .fold(0., f32::max);
        let height = text.split('\n').count() as f32 * size * LINE_HEIGHT;
        let container = self.elements[i]
            .container_id
            .as_deref()
            .and_then(|c| self.index(c));
        let center = container.map(|c| self.elements[c].center());
        let el = &mut self.elements[i];
        el.width = width;
        el.height = height;
        el.rest.insert("originalText".into(), json!(text));
        if let Some(c) = center {
            el.x = c[0] - width / 2.;
            el.y = c[1] - height / 2.;
        }
        self.changed(cx);
    }

    fn erase(&mut self, p: V) {
        if let Some(i) = self.hit(p) {
            let id = self.elements[i].id.clone();
            self.remove(&id);
        }
    }

    /// Rebuilds the selected elements from the pre-drag snapshot (last undo entry).
    fn transform(&mut self, p: V, shift: bool) {
        let Drag::Transform {
            handle,
            lo,
            hi,
            angle,
            start,
            ..
        } = self.drag
        else {
            return;
        };
        let Some(orig) = self.undo.last() else {
            return;
        };
        let c = [(lo[0] + hi[0]) / 2., (lo[1] + hi[1]) / 2.];
        let sel = &self.selected;
        if handle == ROTATE_HANDLE {
            let mut da =
                (p[1] - c[1]).atan2(p[0] - c[0]) - (start[1] - c[1]).atan2(start[0] - c[0]);
            if shift {
                da = (da / (PI / 12.)).round() * (PI / 12.);
            }
            for (el, o) in self.elements.iter_mut().zip(orig) {
                if !sel.contains(&o.id) {
                    continue;
                }
                *el = o.clone();
                let (oc, nc) = (o.center(), rotate(o.center(), c, da));
                el.x += nc[0] - oc[0];
                el.y += nc[1] - oc[1];
                el.angle = (o.angle + da).rem_euclid(2. * PI);
            }
        } else {
            // Work in the box's unrotated frame; the opposite edge/corner is the anchor.
            let q = rotate(p, c, -angle);
            let [sx, sy] = HANDLES[handle];
            let (w, h) = (hi[0] - lo[0], hi[1] - lo[1]);
            let mut a = [
                if sx < 0. { hi[0] } else { lo[0] },
                if sy < 0. { hi[1] } else { lo[1] },
            ];
            let factor = |s: f32, q: f32, a: f32, len: f32| {
                if s == 0. || len < 1e-3 {
                    1.
                } else {
                    ((q - a) * s).max(1.) / len
                }
            };
            let (mut fx, mut fy) = (factor(sx, q[0], a[0], w), factor(sy, q[1], a[1], h));
            if shift {
                if sx == 0. {
                    fx = fy;
                    a[0] = c[0];
                } else if sy == 0. {
                    fy = fx;
                    a[1] = c[1];
                } else {
                    fx = fx.max(fy);
                    fy = fx;
                }
            }
            let scale = |v: V| [a[0] + (v[0] - a[0]) * fx, a[1] + (v[1] - a[1]) * fy];
            // Keep the anchor fixed on screen when the box rotates about its new center.
            let (nlo, nhi) = (scale(lo), scale(hi));
            let nc = [(nlo[0] + nhi[0]) / 2., (nlo[1] + nhi[1]) / 2.];
            let (before, after) = (rotate(a, c, angle), rotate(a, nc, angle));
            let off = [before[0] - after[0], before[1] - after[1]];
            for (el, o) in self.elements.iter_mut().zip(orig) {
                if !sel.contains(&o.id) {
                    continue;
                }
                *el = o.clone();
                if o.is_linear() {
                    let [x, y] = scale([o.x, o.y]);
                    el.x = x + off[0];
                    el.y = y + off[1];
                    el.points = o.points.iter().map(|q| [q[0] * fx, q[1] * fy]).collect();
                    el.fit_points();
                    continue;
                }
                // Text scales uniformly (font size follows the vertical factor).
                let (ex, ey) = if o.kind == "text" { (fy, fy) } else { (fx, fy) };
                if let Some(fs) = &mut el.font_size {
                    *fs *= fy;
                }
                el.width = o.width * ex;
                el.height = o.height * ey;
                let oc = scale(o.center());
                el.x = oc[0] + off[0] - el.width / 2.;
                el.y = oc[1] + off[1] - el.height / 2.;
            }
        }
        self.sync_bound_text();
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.drag = Drag::None;
        self.commit_text(window, cx);
        let p = self.scene(ev.position);
        let stroke = STROKES[self.stroke];
        let kind = match self.tool {
            Tool::Select => {
                let hit = self.hit(p);
                if ev.click_count >= 2 {
                    let target = hit.map(|i| {
                        let el = &self.elements[i];
                        (
                            el.id.clone(),
                            el.kind == "text",
                            el.is_linear() || el.kind == "image",
                            el.center(),
                        )
                    });
                    match target {
                        Some((id, true, ..)) => {
                            self.checkpoint();
                            self.editing = Some(id);
                        }
                        Some((id, false, false, c)) => self.start_text(c, Some(id)),
                        _ => self.start_text(p, None),
                    }
                } else {
                    let zoom = self.zoom;
                    let handle = self.selection_box().and_then(|(lo, hi, angle)| {
                        (0..HANDLES.len())
                            .find(|&i| dist(handle_pos(lo, hi, angle, zoom, i), p) < 7. / zoom)
                            .map(|handle| (handle, lo, hi, angle))
                    });
                    if let Some((handle, lo, hi, angle)) = handle {
                        self.checkpoint();
                        self.drag = Drag::Transform {
                            handle,
                            lo,
                            hi,
                            angle,
                            start: p,
                            moved: false,
                        };
                    } else if let Some(i) = hit {
                        let id = self.elements[i].id.clone();
                        let already = self.selected.contains(&id);
                        if ev.modifiers.shift && already {
                            self.selected.retain(|s| s != &id);
                        } else if ev.modifiers.shift {
                            self.selected.push(id);
                        } else if !already {
                            self.selected = vec![id];
                        }
                        if !self.selected.is_empty() {
                            self.checkpoint();
                            self.drag = Drag::Move {
                                last: p,
                                moved: false,
                            };
                        }
                    } else {
                        if !ev.modifiers.shift {
                            self.selected.clear();
                        }
                        self.drag = Drag::Marquee { start: p, end: p };
                    }
                }
                cx.notify();
                return;
            }
            Tool::Eraser => {
                self.checkpoint();
                self.drag = Drag::Erase;
                self.erase(p);
                self.changed(cx);
                return;
            }
            Tool::Text => {
                self.start_text(p, None);
                self.tool = Tool::Select;
                cx.notify();
                return;
            }
            Tool::Rectangle => "rectangle",
            Tool::Diamond => "diamond",
            Tool::Ellipse => "ellipse",
            Tool::Arrow => "arrow",
            Tool::Line => "line",
            Tool::Draw => "freedraw",
        };
        self.checkpoint();
        let bg = if matches!(self.tool, Tool::Arrow | Tool::Line | Tool::Draw) {
            "transparent"
        } else {
            FILLS[self.fill]
        };
        let mut el = Element::new(kind, p, stroke, bg);
        el.points = match self.tool {
            Tool::Arrow | Tool::Line => vec![[0., 0.], [0., 0.]],
            Tool::Draw => vec![[0., 0.]],
            _ => Vec::new(),
        };
        self.selected = vec![el.id.clone()];
        self.elements.push(el);
        self.drag = Drag::Create { start: p };
        cx.notify();
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = Some(ev.position);
        if ev.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let p = self.scene(ev.position);
        if matches!(self.drag, Drag::Erase) {
            self.erase(p);
            return self.changed(cx);
        }
        match &mut self.drag {
            Drag::None | Drag::Erase => return,
            Drag::Marquee { end, .. } => {
                *end = p;
                return cx.notify();
            }
            Drag::Transform { moved, .. } => {
                *moved = true;
                self.transform(p, ev.modifiers.shift);
            }
            Drag::Move { last, moved } => {
                let d = [p[0] - last[0], p[1] - last[1]];
                *last = p;
                *moved = true;
                let sel = &self.selected;
                for el in self.elements.iter_mut().filter(|e| {
                    sel.contains(&e.id) || e.container_id.as_ref().is_some_and(|c| sel.contains(c))
                }) {
                    el.x += d[0];
                    el.y += d[1];
                }
            }
            Drag::Create { start } => {
                let start = *start;
                let Some(el) = self.elements.last_mut() else {
                    return;
                };
                match el.kind.as_str() {
                    "line" | "arrow" => {
                        el.points[1] = [p[0] - el.x, p[1] - el.y];
                        el.fit_points();
                    }
                    "freedraw" => {
                        let q = [p[0] - el.x, p[1] - el.y];
                        if el.points.last().is_none_or(|l| dist(*l, q) > 1.) {
                            el.points.push(q);
                        }
                        el.fit_points();
                    }
                    _ => {
                        el.x = start[0].min(p[0]);
                        el.y = start[1].min(p[1]);
                        el.width = (p[0] - start[0]).abs();
                        el.height = (p[1] - start[1]).abs();
                    }
                }
            }
        }
        self.changed(cx);
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Move { moved: false, .. } | Drag::Transform { moved: false, .. } => {
                self.undo.pop();
            }
            Drag::Marquee { start, end } => {
                let (lo, hi) = (
                    [start[0].min(end[0]), start[1].min(end[1])],
                    [start[0].max(end[0]), start[1].max(end[1])],
                );
                let inside =
                    |q: &V| q[0] >= lo[0] && q[0] <= hi[0] && q[1] >= lo[1] && q[1] <= hi[1];
                let picked: Vec<String> = self
                    .elements
                    .iter()
                    .filter(|e| {
                        e.container_id.is_none()
                            && e.corners().iter().all(inside)
                            && !self.selected.contains(&e.id)
                    })
                    .map(|e| e.id.clone())
                    .collect();
                self.selected.extend(picked);
            }
            Drag::Erase => {
                if self.undo.last().map(Vec::len) == Some(self.elements.len()) {
                    self.undo.pop();
                }
            }
            Drag::Create { .. } => {
                let tiny = self
                    .elements
                    .last()
                    .is_some_and(|e| e.kind != "freedraw" && e.width < 2. && e.height < 2.);
                if tiny {
                    self.elements.pop();
                    self.undo.pop();
                    self.selected.clear();
                }
                if self.tool != Tool::Draw {
                    self.tool = Tool::Select;
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let ctrl = k.modifiers.control || k.modifiers.platform;
        if let Some(i) = self.editing.as_deref().and_then(|id| self.index(id)) {
            let text = self.elements[i].text.get_or_insert_default();
            match k.key.as_str() {
                "escape" => return self.commit_text(window, cx),
                "backspace" => {
                    text.pop();
                }
                "enter" => text.push('\n'),
                _ => match &k.key_char {
                    Some(ch) if !ctrl => text.push_str(ch),
                    _ => return,
                },
            }
            cx.stop_propagation();
            return self.changed(cx);
        }
        match (ctrl, k.key.as_str()) {
            (true, "z") => self.history(k.modifiers.shift, cx),
            (true, "y") => self.history(true, cx),
            (true, "a") => {
                self.tool = Tool::Select;
                self.selected = self
                    .elements
                    .iter()
                    .filter(|e| e.container_id.is_none())
                    .map(|e| e.id.clone())
                    .collect();
                cx.notify();
            }
            (true, "d") => self.duplicate(cx),
            (true, "v") => {
                if !self.paste(cx) {
                    return;
                }
            }
            (false, "delete" | "backspace") => {
                if !self.selected.is_empty() {
                    self.checkpoint();
                    for id in std::mem::take(&mut self.selected) {
                        self.remove(&id);
                    }
                    self.changed(cx);
                }
            }
            (false, key) => {
                if let Some((tool, ..)) = TOOLS.iter().find(|t| t.2 == key) {
                    self.tool = *tool;
                    cx.notify();
                } else {
                    return;
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn set_color(&mut self, stroke: Option<usize>, fill: Option<usize>, cx: &mut Context<Self>) {
        if let Some(s) = stroke {
            self.stroke = s;
        }
        if let Some(f) = fill {
            self.fill = f;
        }
        if !self.selected.is_empty() {
            self.checkpoint();
            let sel = &self.selected;
            // Bound text follows its container's stroke colour.
            for el in self.elements.iter_mut().filter(|e| {
                sel.contains(&e.id) || e.container_id.as_ref().is_some_and(|c| sel.contains(c))
            }) {
                if el.kind == "image" {
                    continue;
                }
                if let Some(s) = stroke {
                    el.stroke_color = STROKES[s].into();
                }
                if let Some(f) = fill.filter(|_| !el.is_linear() && el.kind != "text") {
                    el.background_color = FILLS[f].into();
                }
            }
            self.changed(cx);
        }
        cx.notify();
    }

    fn add_sticky(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_text(window, cx);
        let c = self.scene(self.bounds.get().center());
        self.checkpoint();
        let mut note = Element::new(
            "rectangle",
            [c[0] - 100., c[1] - 100.],
            STROKES[self.stroke],
            STICKY,
        );
        note.width = 200.;
        note.height = 200.;
        let id = note.id.clone();
        self.elements.push(note);
        self.start_text(c, Some(id));
        self.undo.pop(); // start_text checkpointed again; keep one undo step.
        if let Some(i) = self.editing.as_deref().and_then(|id| self.index(id)) {
            self.elements[i].text = Some("Catatan".into());
        }
        self.commit_text(window, cx);
        self.tool = Tool::Select;
    }

    fn write(&mut self, path: PathBuf, cx: &mut Context<Self>) -> Result<()> {
        let mut rest = self.rest.clone();
        rest.insert("type".into(), json!("excalidraw"));
        rest.insert("version".into(), json!(2));
        rest.entry("source").or_insert(json!("zed-whiteboard"));
        let app = rest.entry("appState").or_insert(json!({}));
        if !app.is_object() {
            *app = json!({});
        }
        app["zoom"] = json!({ "value": self.zoom });
        app["scrollX"] = json!(self.scroll[0]);
        app["scrollY"] = json!(self.scroll[1]);
        // Forget images nothing (not even undo/redo) refers to; write only those on the board.
        let referenced: HashSet<String> = self
            .undo
            .iter()
            .chain(&self.redo)
            .flatten()
            .filter_map(|e| e.file_id().map(String::from))
            .collect();
        let on_board: HashSet<&str> = self.elements.iter().filter_map(Element::file_id).collect();
        self.files
            .retain(|id, _| referenced.contains(id) || on_board.contains(id.as_str()));
        let files: Map<String, Value> = self
            .files
            .iter()
            .filter(|(id, _)| on_board.contains(id.as_str()))
            .map(|(id, f)| (id.clone(), f.to_value()))
            .collect();
        rest.insert("files".into(), Value::Object(files));
        let doc = Doc {
            elements: self.elements.clone(),
            rest,
        };
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        self.dirty = false;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        Ok(())
    }
}

impl EventEmitter<ItemEvent> for Whiteboard {}

impl Focusable for Whiteboard {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Whiteboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ponytail: clones the scene each frame; switch to Rc<Vec<Element>> if boards get huge.
        let elements = self.elements.clone();
        let editing = self.editing.clone();
        let selected = self.selected.clone();
        let sel_box = self.selection_box();
        let marquee = match self.drag {
            Drag::Marquee { start, end } => Some((start, end)),
            _ => None,
        };
        let images: HashMap<String, Arc<Image>> = self
            .files
            .iter()
            .filter_map(|(id, f)| Some((id.clone(), f.image.clone()?)))
            .collect();
        let (scroll, zoom) = (self.scroll, self.zoom);
        let bounds_cell = self.bounds.clone();

        let board = canvas(
            move |bounds, _, _| bounds_cell.set(bounds),
            move |bounds, _, window, cx| {
                window.paint_quad(fill(bounds, rgb(0x121212)));
                let dot = gpui::hsla(0., 0., 1., 0.12);
                let step = 20. * zoom;
                let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
                // Skip the grid when zoomed far out instead of painting thousands of dots.
                let mut x = if step < 6. {
                    w
                } else {
                    (scroll[0] * zoom).rem_euclid(step)
                };
                while x < w {
                    let mut y = (scroll[1] * zoom).rem_euclid(step);
                    while y < h {
                        let o = bounds.origin + point(px(x), px(y));
                        window.paint_quad(fill(Bounds::new(o, size(px(1.5), px(1.5))), dot));
                        y += step;
                    }
                    x += step;
                }
                let origin = bounds.origin + point(px(scroll[0] * zoom), px(scroll[1] * zoom));
                let mut painter = Painter {
                    origin,
                    zoom,
                    images: &images,
                    window,
                    cx,
                };
                for el in &elements {
                    painter.element(el, &elements, editing.as_deref());
                }
                if selected.len() > 1 {
                    for el in elements.iter().filter(|e| selected.contains(&e.id)) {
                        let (lo, hi) = el.bbox();
                        painter.dashed_box(lo, hi, el.angle);
                    }
                }
                if let Some((lo, hi, angle)) = sel_box {
                    painter.dashed_box(lo, hi, angle);
                    for i in 0..HANDLES.len() {
                        let c = painter.pt(handle_pos(lo, hi, angle, zoom, i));
                        let b = Bounds::new(c - point(px(4.), px(4.)), size(px(8.), px(8.)));
                        painter.window.paint_quad(fill(b, rgb(0xa8a5ff)));
                    }
                }
                if let Some((a, b)) = marquee {
                    let lo = painter.pt([a[0].min(b[0]), a[1].min(b[1])]);
                    let sz = size(
                        px((a[0] - b[0]).abs() * zoom),
                        px((a[1] - b[1]).abs() * zoom),
                    );
                    painter
                        .window
                        .paint_quad(fill(Bounds::new(lo, sz), gpui::hsla(0.67, 1., 0.8, 0.1)));
                }
            },
        )
        .size_full();

        let button = |id: &'static str, label: SharedString, active: bool| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_md()
                .text_sm()
                .text_color(rgb(0xe3e3e8))
                .when(active, |d| d.bg(rgb(0x403e6a)))
                .hover(|d| d.bg(rgb(0x31303b)))
                .child(label)
        };
        let swatch = |id: SharedString, hex: &str, active: bool| {
            div()
                .id(id)
                .size_5()
                .rounded_sm()
                .border_1()
                .border_color(if active { rgb(0xa8a5ff) } else { rgb(0x46464f) })
                .when_some(color(hex, 100.), |d, c| d.bg(c))
        };

        let toolbar = div()
            .occlude()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_lg()
            .bg(rgb(0x232329))
            .border_1()
            .border_color(rgb(0x31303b))
            .children(TOOLS.iter().map(|&(tool, label, _)| {
                button(label, label.into(), self.tool == tool).on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.commit_text(window, cx);
                        this.tool = tool;
                        cx.notify();
                    },
                ))
            }))
            .child(div().w_px().h_5().mx_1().bg(rgb(0x46464f)))
            .children(STROKES.iter().enumerate().map(|(i, hex)| {
                swatch(format!("stroke-{i}").into(), hex, self.stroke == i)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_color(Some(i), None, cx)))
            }))
            .child(div().w_px().h_5().mx_1().bg(rgb(0x46464f)))
            .children(FILLS.iter().enumerate().map(|(i, hex)| {
                swatch(format!("fill-{i}").into(), hex, self.fill == i)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_color(None, Some(i), cx)))
            }))
            .child(div().w_px().h_5().mx_1().bg(rgb(0x46464f)))
            .child(
                button("sticky", "Sticky note".into(), false)
                    .on_click(cx.listener(|this, _, window, cx| this.add_sticky(window, cx))),
            );

        let zoom_control =
            div()
                .occlude()
                .absolute()
                .bottom_3()
                .left_3()
                .flex()
                .items_center()
                .p_1()
                .rounded_lg()
                .bg(rgb(0x232329))
                .border_1()
                .border_color(rgb(0x31303b))
                .child(button("zoom-out", "\u{2212}".into(), false).on_click(
                    cx.listener(|this, _, _, cx| this.zoom_to(this.zoom - 0.1, None, cx)),
                ))
                .child(
                    button(
                        "zoom-reset",
                        format!("{:.0}%", self.zoom * 100.).into(),
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_to(1., None, cx))),
                )
                .child(button("zoom-in", "+".into(), false).on_click(
                    cx.listener(|this, _, _, cx| this.zoom_to(this.zoom + 0.1, None, cx)),
                ));

        div()
            .track_focus(&self.focus)
            .key_context("Whiteboard")
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(FONT)
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &workspace::Unfollow, window, cx| {
                if this.editing.is_some() {
                    this.commit_text(window, cx);
                } else if !this.selected.is_empty() {
                    this.selected.clear();
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            // Ctrl+= / Ctrl+- / Ctrl+0 are global font-size bindings; claim them while focused.
            .on_action(
                cx.listener(|this, _: &zed_actions::IncreaseBufferFontSize, _, cx| {
                    this.zoom_to(this.zoom + 0.1, None, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &zed_actions::DecreaseBufferFontSize, _, cx| {
                    this.zoom_to(this.zoom - 0.1, None, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &zed_actions::ResetBufferFontSize, _, cx| {
                    this.zoom_to(1., None, cx)
                }),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                let d = ev.delta.pixel_delta(px(20.));
                if ev.modifiers.control || ev.modifiers.platform {
                    let z = this.zoom * (f32::from(d.y) * 0.004).exp();
                    return this.zoom_to(z, Some(ev.position), cx);
                }
                this.scroll[0] += f32::from(d.x) / this.zoom;
                this.scroll[1] += f32::from(d.y) / this.zoom;
                cx.notify();
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_image_files(paths.paths(), cx);
            }))
            .child(board)
            .child(
                div()
                    .absolute()
                    .top_3()
                    .left_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(toolbar),
            )
            .child(zoom_control)
    }
}

impl Item for Whiteboard {
    type Event = ItemEvent;

    fn to_item_events(event: &ItemEvent, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.file
            .as_ref()
            .and_then(|f| {
                f.read(cx)
                    .path
                    .path
                    .file_name()
                    .map(|n| SharedString::from(n.to_string()))
            })
            .unwrap_or_else(|| "Whiteboard".into())
    }

    fn suggested_filename(&self, _: &App) -> SharedString {
        "Untitled.excalidraw".into()
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        if let Some(file) = &self.file {
            f(file.entity_id(), file.read(cx));
        }
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    fn can_save(&self, _: &App) -> bool {
        self.file.is_some()
    }

    fn can_save_as(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _: SaveOptions,
        _: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.commit_text(window, cx);
        let Some(path) = self.file.as_ref().map(|f| f.read(cx).abs_path.clone()) else {
            return Task::ready(Err(anyhow!("whiteboard has no file")));
        };
        Task::ready(self.write(path, cx))
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.commit_text(window, cx);
        let Some(abs_path) = project.read(cx).absolute_path(&path, cx) else {
            return Task::ready(Err(anyhow!("cannot resolve save path")));
        };
        self.file = Some(cx.new(|_| BoardFile {
            path,
            abs_path: abs_path.clone(),
            entry_id: None,
            doc: None,
        }));
        Task::ready(self.write(abs_path, cx))
    }

    fn reload(
        &mut self,
        _: Entity<Project>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let Some(path) = self.file.as_ref().map(|f| f.read(cx).abs_path.clone()) else {
            return Task::ready(Ok(()));
        };
        Task::ready(read_doc(&path).map(|doc| {
            self.checkpoint();
            self.set_doc(doc);
            self.selected.clear();
            self.dirty = false;
            cx.emit(ItemEvent::UpdateTab);
            cx.notify();
        }))
    }
}

impl workspace::item::ProjectItem for Whiteboard {
    type Item = BoardFile;

    fn for_project_item(
        _: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<BoardFile>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let doc = item
            .update(cx, |file, _| file.doc.take())
            .unwrap_or_default();
        Self::new(Some(item), doc, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_unknown_fields() {
        let src = r##"{"type":"excalidraw","version":2,"appState":{"gridSize":20},"elements":[
            {"id":"a","type":"rectangle","x":1,"y":2,"width":3,"height":4,"strokeColor":"#1e1e1e",
             "groupIds":["g"],"roundness":null},
            {"id":"b","type":"freedraw","x":0,"y":0,"points":[[0,0],[5,5]],"pressures":[],"simulatePressure":true}]}"##;
        let doc: Doc = serde_json::from_str(src).unwrap();
        let out = serde_json::to_value(&doc).unwrap();
        assert_eq!(out["appState"]["gridSize"], 20);
        assert_eq!(out["elements"][0]["groupIds"][0], "g");
        assert_eq!(out["elements"][0]["strokeColor"], "#1e1e1e");
        assert_eq!(out["elements"][1]["points"][1][0], 5.0);
    }

    #[test]
    fn freehand_outline_is_closed_polygon() {
        let pts: Vec<V> = (0..20)
            .map(|i| [i as f32 * 5., (i as f32).sin() * 10.])
            .collect();
        let outline = freehand(&pts, 8.5);
        assert!(outline.len() > 20);
        assert!(outline.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
        assert!(!freehand(&[[3., 3.]], 8.5).is_empty());
    }

    #[test]
    fn image_file_roundtrips_data_url() {
        // 1x1 PNG.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let url = format!("data:image/png;base64,{png}");
        let f = ImageFile::from_value(
            json!({"mimeType": "image/png", "id": "f", "dataURL": url, "created": 1}),
        );
        assert!(f.image.is_some() && !f.meta.contains_key("dataURL"));
        assert_eq!(f.to_value()["dataURL"], json!(url));
    }

    #[test]
    fn handles_surround_the_box() {
        let (lo, hi) = ([0., 0.], [100., 50.]);
        let br = handle_pos(lo, hi, 0., 1., 4);
        assert!(br[0] > 100. && br[1] > 50.);
        let tl = handle_pos(lo, hi, 0., 1., 0);
        assert!(tl[0] < 0. && tl[1] < 0.);
        assert!(handle_pos(lo, hi, 0., 1., ROTATE_HANDLE)[1] < tl[1]);
    }
}
