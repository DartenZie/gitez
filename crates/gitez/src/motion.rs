//! Live marks: the three animations of the agent-thread boards (`C-1` to
//! `C-6`), shared by every view that shows a thread at work.
//!
//! - **Pulse**: a ring that grows from a running lane's head and fades
//!   (`scale .75 → 1.7`, `opacity .9 → 0`, 1.8 s, ease-out).
//! - **Spin**: a dashed ring, or a spinner icon, that turns (3.2 s a turn,
//!   linear): a preparing lane's head, a preparing thread's icon, the step
//!   the agent is on.
//! - **Blink**: a status dot that fades to 35 % and back (1.6 s,
//!   ease-in-out): the dot of a Running or Preparing chip, the "active"
//!   dots, "live".
//!
//! # Why a layer
//!
//! GPUI redraws a view by rendering it and all its ancestors again, so an
//! animation inside the history would lay out the visible rows of the list
//! (and the rest of the window) thirty times a second. Instead the window's
//! content is a cached view inside a [`Shell`], and the moving parts are
//! painted by a [`LiveLayer`] on top of it:
//!
//! 1. When the content really paints, each mark registers where it is
//!    (window coordinates and the clip it is painted in) instead of painting
//!    its moving part; [`content_start`] clears the previous registrations
//!    first.
//! 2. The layer paints the registered marks at the current time and, while
//!    any is visible, asks itself (only) to be painted again a frame later.
//!    The content is replayed from GPUI's cache: no render, no layout.
//! 3. A mark scrolled out of view is not painted by its list, so it does not
//!    register; with no mark left the layer requests no frame, and an idle
//!    window stays idle. A hidden window requests none either; the layer
//!    starts again when it shows.
//!
//! The layer paints over the content, so an opaque element that can cover a
//! mark (a card over the history) marks itself with [`occluder`], and the
//! layer clips marks away from it. A translucent cover (the palette's
//! backdrop) instead sets `still` in [`content_start`]: marks then paint
//! their still form in the content, as with reduced motion.
//!
//! Marks never animate inside deferred elements (popovers, menus): those
//! paint after the layer. The boards keep the popover's marks still anyway.
//!
//! # Reduced motion
//!
//! gpui-kit's `init` reads the system's "Reduce motion" setting into
//! [`App::reduce_motion`] (macOS: `NSWorkspace
//! .accessibilityDisplayShouldReduceMotion`); the layer reads it again when
//! the window becomes active, so a change made in System Settings applies on
//! return. With it on, every mark paints its still form and no frame is
//! requested.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::*;

use crate::theme;

/// One turn of the pulse ring.
pub const PULSE_PERIOD: Duration = Duration::from_millis(1800);
/// One turn of a spinning ring or icon.
pub const SPIN_PERIOD: Duration = Duration::from_millis(3200);
/// One fade out and back of a blinking dot.
pub const BLINK_PERIOD: Duration = Duration::from_millis(1600);
/// Time between frames while something moves: at most 30 a second (about
/// 25 in the app, as each waits for the display's next refresh). The
/// motions are slow (a pulse ring grows about 0.2 px a frame), so more
/// frames would cost without showing.
pub const FRAME: Duration = Duration::from_micros(33_333);

/// The pulse ring, as on the board: radius 8 and stroke 2 at scale 1.
const PULSE_RADIUS: f32 = 8.;
const PULSE_WIDTH: f32 = 2.;
const PULSE_SCALE: (f32, f32) = (0.75, 1.7);
const PULSE_OPACITY: f32 = 0.9;
/// The still form of the pulse: a faint ring around the node.
const HALO_RADIUS: Pixels = px(10.);
const HALO_WIDTH: Pixels = px(2.);
const HALO_ALPHA: f32 = 0.35;
/// The dashes of a spinning ring.
const SPIN_DASHES: [Pixels; 2] = [px(3.), px(2.5)];
/// The low point of a blink.
const BLINK_LOW: f32 = 0.35;

/// A mark whose part moves, where it is painted (window coordinates).
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// A ring growing from `center` and fading, in `color` (`0xRRGGBB`);
    /// the node itself is painted by its owner.
    Pulse { center: Point<Pixels>, color: u32 },
    /// A dashed ring `width` wide, its outer edge at `radius`, turning
    /// around `center`; filled with `fill` when given.
    Spin {
        center: Point<Pixels>,
        radius: Pixels,
        width: Pixels,
        color: u32,
        fill: Option<u32>,
    },
    /// A dot of `radius` fading out and back.
    Blink {
        center: Point<Pixels>,
        radius: Pixels,
        color: u32,
    },
    /// The icon at `path` turning within `bounds`.
    Spinner {
        bounds: Bounds<Pixels>,
        path: SharedString,
        color: u32,
    },
}

impl Mark {
    /// The most the mark ever covers.
    fn extent(&self) -> Bounds<Pixels> {
        let around = |center: Point<Pixels>, radius: Pixels| {
            Bounds::centered_at(center, size(radius * 2., radius * 2.))
        };
        match self {
            Mark::Pulse { center, .. } => around(
                *center,
                px((PULSE_RADIUS + PULSE_WIDTH / 2.) * PULSE_SCALE.1 + 1.).max(HALO_RADIUS),
            ),
            Mark::Spin { center, radius, .. } => around(*center, *radius + px(1.)),
            Mark::Blink { center, radius, .. } => around(*center, *radius + px(1.)),
            Mark::Spinner { bounds, .. } => bounds.dilate(px(1.)),
        }
    }

    /// Paints the mark as it stands still: with reduced motion, where it
    /// cannot move, or in a window without a [`LiveLayer`].
    pub fn paint_still(&self, window: &mut Window, cx: &App) {
        match self {
            Mark::Pulse { center, color } => window.paint_quad(quad(
                circle(*center, HALO_RADIUS),
                HALO_RADIUS,
                transparent_black(),
                HALO_WIDTH,
                theme::with_alpha(*color, HALO_ALPHA),
                BorderStyle::default(),
            )),
            Mark::Spin {
                center,
                radius,
                width,
                color,
                fill,
            } => window.paint_quad(quad(
                circle(*center, *radius),
                *radius,
                fill.map_or_else(transparent_black, |fill| rgb(fill).into()),
                *width,
                rgb(*color),
                BorderStyle::Dashed,
            )),
            Mark::Blink {
                center,
                radius,
                color,
            } => paint_dot(*center, *radius, rgb(*color).into(), window),
            Mark::Spinner {
                bounds,
                path,
                color,
            } => paint_icon(*bounds, path, 0., *color, window, cx),
        }
    }

    /// Paints the mark `elapsed` into its animation.
    pub fn paint_at(&self, elapsed: Duration, window: &mut Window, cx: &App) {
        match self {
            Mark::Pulse { center, color } => {
                let (scale, opacity) = pulse(elapsed);
                let outer = px((PULSE_RADIUS + PULSE_WIDTH / 2.) * scale);
                window.paint_quad(quad(
                    circle(*center, outer),
                    outer,
                    transparent_black(),
                    px(PULSE_WIDTH * scale),
                    theme::with_alpha(*color, opacity),
                    BorderStyle::default(),
                ));
            }
            Mark::Spin {
                center,
                radius,
                width,
                color,
                fill,
            } => {
                if let Some(fill) = fill {
                    window.paint_quad(fill_circle(*center, *radius, rgb(*fill).into()));
                }
                if let Some(path) =
                    dashed_ring(*center, *radius - *width / 2., *width, spin(elapsed))
                {
                    window.paint_path(path, rgb(*color));
                }
            }
            Mark::Blink {
                center,
                radius,
                color,
            } => paint_dot(
                *center,
                *radius,
                theme::with_alpha(*color, blink(elapsed)).into(),
                window,
            ),
            Mark::Spinner {
                bounds,
                path,
                color,
            } => paint_icon(*bounds, path, spin(elapsed), *color, window, cx),
        }
    }
}

fn circle(center: Point<Pixels>, radius: Pixels) -> Bounds<Pixels> {
    Bounds::centered_at(center, size(radius * 2., radius * 2.))
}

fn fill_circle(center: Point<Pixels>, radius: Pixels, color: Hsla) -> PaintQuad {
    fill(circle(center, radius), color).corner_radii(radius)
}

fn paint_dot(center: Point<Pixels>, radius: Pixels, color: Hsla, window: &mut Window) {
    window.paint_quad(fill_circle(center, radius, color));
}

/// A ring of dashes around `center` with its centre line at `radius`, the
/// dash pattern starting `angle` radians clockwise from three o'clock, as
/// an SVG circle turned by `angle`.
fn dashed_ring(
    center: Point<Pixels>,
    radius: Pixels,
    width: Pixels,
    angle: f32,
) -> Option<Path<Pixels>> {
    let at = |angle: f32| {
        point(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        )
    };
    let radii = point(radius, radius);
    let mut builder = PathBuilder::stroke(width).dash_array(&SPIN_DASHES);
    builder.move_to(at(angle));
    builder.arc_to(radii, px(0.), false, true, at(angle + PI));
    builder.arc_to(radii, px(0.), false, true, at(angle + TAU));
    builder.build().ok()
}

/// Paints the SVG icon at `path` in `bounds`, turned `angle` radians
/// clockwise around its centre.
fn paint_icon(
    bounds: Bounds<Pixels>,
    path: &SharedString,
    angle: f32,
    color: u32,
    window: &mut Window,
    cx: &App,
) {
    let scale = window.scale_factor();
    let center = bounds.center();
    let transformation = TransformationMatrix::unit()
        .translate(center.scale(scale))
        .rotate(radians(angle))
        .translate(center.scale(-scale));
    window
        .paint_svg(
            bounds,
            path.clone(),
            None,
            transformation,
            rgb(color).into(),
            cx,
        )
        .ok();
}

/// How far into a repeating animation of `period` `elapsed` is, from 0 to 1.
fn progress(elapsed: Duration, period: Duration) -> f32 {
    let period = period.as_nanos();
    (elapsed.as_nanos() % period) as f32 / period as f32
}

/// The pulse ring's scale and opacity `elapsed` into the animation.
pub fn pulse(elapsed: Duration) -> (f32, f32) {
    let eased = cubic_bezier((0., 0., 0.58, 1.), progress(elapsed, PULSE_PERIOD));
    let (from, to) = PULSE_SCALE;
    (from + (to - from) * eased, PULSE_OPACITY * (1. - eased))
}

/// The angle of a spinning mark, in radians clockwise.
pub fn spin(elapsed: Duration) -> f32 {
    TAU * progress(elapsed, SPIN_PERIOD)
}

/// A blinking dot's opacity: down to 35 % at half the period and back, each
/// half eased in and out (CSS `@keyframes blink{50%{opacity:.35}}`).
pub fn blink(elapsed: Duration) -> f32 {
    let ease = |t| cubic_bezier((0.42, 0., 0.58, 1.), t);
    let t = progress(elapsed, BLINK_PERIOD);
    if t < 0.5 {
        1. - (1. - BLINK_LOW) * ease(t * 2.)
    } else {
        BLINK_LOW + (1. - BLINK_LOW) * ease(t * 2. - 1.)
    }
}

/// CSS `cubic-bezier(x1, y1, x2, y2)` at `x` (0 to 1).
fn cubic_bezier((x1, y1, x2, y2): (f32, f32, f32, f32), x: f32) -> f32 {
    let at = |a: f32, b: f32, t: f32| {
        let u = 1. - t;
        3. * u * u * t * a + 3. * u * t * t * b + t * t * t
    };
    // Bisection on the curve's x, which rises monotonically for 0 ≤ x1, x2 ≤ 1.
    let (mut low, mut high) = (0f32, 1f32);
    for _ in 0..24 {
        let middle = (low + high) / 2.;
        if at(x1, x2, middle) < x {
            low = middle;
        } else {
            high = middle;
        }
    }
    at(y1, y2, (low + high) / 2.)
}

/// What one window's content registered at its last paint.
#[derive(Default)]
struct Registry {
    /// Marks with the clip they were painted in and their paint order.
    marks: Vec<(Mark, Bounds<Pixels>, u32)>,
    /// Opaque elements over the content, with their paint order.
    occluders: Vec<(Bounds<Pixels>, u32)>,
    next: u32,
}

/// Counts for tests and `GITEZ_DEBUG_THREADS`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Frames the layer asked for.
    pub frames: u64,
    /// Times the content really painted (not replayed from the cache).
    pub content_paints: u64,
    /// Times the layer painted.
    pub layer_paints: u64,
}

/// One window's live layer, shared between its content and its layer.
#[derive(Default)]
struct Layer {
    registry: RefCell<Registry>,
    /// Set by the content: marks stand still under a translucent cover.
    still: Cell<bool>,
    /// A frame is asked for and not yet painted.
    pending: Cell<bool>,
    stats: Cell<Stats>,
    /// For `GITEZ_DEBUG_THREADS`: whether the last paint moved something,
    /// and frames counted since `second` began.
    moving: Cell<bool>,
    second: Cell<Option<(Duration, u64)>>,
}

impl Layer {
    fn count(&self, change: impl FnOnce(&mut Stats)) {
        let mut stats = self.stats.get();
        change(&mut stats);
        self.stats.set(stats);
    }

    fn push_order(registry: &mut Registry) -> u32 {
        registry.next += 1;
        registry.next
    }
}

/// The layers of open windows, and the time animations count from.
struct Motion {
    layers: HashMap<WindowId, Rc<Layer>>,
    epoch: std::time::Instant,
    /// Screenshots paint every mark at this point of its animation and
    /// request no frames.
    fixed: Option<Duration>,
}

impl Global for Motion {}

fn layer_of(window: &Window, cx: &App) -> Option<Rc<Layer>> {
    cx.try_global::<Motion>()?
        .layers
        .get(&window.window_handle().window_id())
        .cloned()
}

/// The layer that will paint marks of this window now, if they move.
fn moving_layer(window: &Window, cx: &App) -> Option<Rc<Layer>> {
    if cx.reduce_motion() {
        return None;
    }
    layer_of(window, cx).filter(|layer| !layer.still.get())
}

/// Paints `mark` from a canvas of the content: registers it with the
/// window's layer, which moves it, or paints it still.
pub fn paint(mark: Mark, window: &mut Window, cx: &mut App) {
    match moving_layer(window, cx) {
        Some(layer) => {
            let clip = window.content_mask().bounds;
            let mut registry = layer.registry.borrow_mut();
            let order = Layer::push_order(&mut registry);
            registry.marks.push((mark, clip, order));
        }
        None => mark.paint_still(window, cx),
    }
}

/// A `size`-square element painting the mark `place` builds from its
/// bounds.
fn mark_element(
    size: Pixels,
    place: impl Fn(Bounds<Pixels>) -> Mark + 'static,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| paint(place(bounds), window, cx),
    )
    .size(size)
    .flex_none()
}

/// A dot `size` across in `color` that blinks: the dot of a Running or
/// Preparing chip, the "active" dots and "live".
pub fn blinking_dot(size: Pixels, color: u32) -> impl IntoElement {
    mark_element(size, move |bounds| Mark::Blink {
        center: bounds.center(),
        radius: size / 2.,
        color,
    })
}

/// A dashed ring filling a `size` square, its stroke `width` wide, that
/// turns: a preparing thread's icon.
pub fn spinning_ring(size: Pixels, width: Pixels, color: u32) -> impl IntoElement {
    mark_element(size, move |bounds| Mark::Spin {
        center: bounds.center(),
        radius: size / 2.,
        width,
        color,
        fill: None,
    })
}

/// The icon at `path`, `size` square, turning: the step the agent is on.
pub fn spinner(path: SharedString, size: Pixels, color: u32) -> impl IntoElement {
    mark_element(size, move |bounds| Mark::Spinner {
        bounds,
        path: path.clone(),
        color,
    })
}

/// The first child of a window's content: clears what the content
/// registered at its previous paint. With `still`, marks of this paint
/// stand still (the content is under a translucent cover).
pub fn content_start(still: bool) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, cx| {
            if let Some(layer) = layer_of(window, cx) {
                *layer.registry.borrow_mut() = Registry::default();
                layer.still.set(still);
                layer.count(|stats| stats.content_paints += 1);
            }
        },
    )
    .absolute()
    .size_0()
}

/// Marks its parent as an opaque cover of the content: the layer does not
/// paint marks under it.
pub fn occluder() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |bounds, _, window, cx| {
            if let Some(layer) = layer_of(window, cx) {
                let mut registry = layer.registry.borrow_mut();
                let order = Layer::push_order(&mut registry);
                registry.occluders.push((bounds, order));
            }
        },
    )
    .absolute()
    .size_full()
}

/// Sets the point every mark is painted at, and stops frames, for
/// screenshots; `None` lets marks move again.
#[cfg(test)]
pub fn set_fixed_time(time: Option<Duration>, cx: &mut App) {
    init(cx);
    cx.global_mut::<Motion>().fixed = time;
}

/// This window's counts, if it has a layer.
#[cfg(test)]
pub fn stats(window: &Window, cx: &App) -> Option<Stats> {
    layer_of(window, cx).map(|layer| layer.stats.get())
}

/// The marks this window's content registered at its last paint.
#[cfg(test)]
pub fn marks(window: &Window, cx: &App) -> Vec<Mark> {
    layer_of(window, cx).map_or_else(Vec::new, |layer| {
        layer
            .registry
            .borrow()
            .marks
            .iter()
            .map(|(mark, _, _)| mark.clone())
            .collect()
    })
}

/// The marks this window's content registered at its last paint, each
/// with the clip it was painted in: a mark inside a scrolled area is
/// clipped to that area.
#[cfg(test)]
pub fn clipped_marks(window: &Window, cx: &App) -> Vec<(Mark, Bounds<Pixels>)> {
    layer_of(window, cx).map_or_else(Vec::new, |layer| {
        layer
            .registry
            .borrow()
            .marks
            .iter()
            .map(|(mark, clip, _)| (mark.clone(), *clip))
            .collect()
    })
}

fn init(cx: &mut App) {
    if !cx.has_global::<Motion>() {
        cx.set_global(Motion {
            layers: HashMap::new(),
            epoch: cx.background_executor().now(),
            fixed: None,
        });
    }
}

/// Opens a window like `gpui_kit::open_window`, with the content `build`
/// returns cached in a [`Shell`] under a [`LiveLayer`].
pub fn open_window<V: Render>(
    options: WindowOptions,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> anyhow::Result<(AnyWindowHandle, Entity<V>)> {
    let mut built = None;
    let (window, _) = gpui_kit::open_window(options, cx, |window, cx| {
        let content = build(window, cx);
        built = Some(content.clone());
        cx.new(|cx| Shell::new(content.into(), window, cx))
    })?;
    Ok((window, built.expect("open_window ran its build closure")))
}

/// A window's root under gpui-kit's `Root`: the content, cached, and the
/// live layer over it.
pub struct Shell {
    content: AnyView,
    layer: Entity<LiveLayer>,
}

impl Shell {
    pub fn new(content: AnyView, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let layer = cx.new(|cx| LiveLayer::new(window, cx));
        Self { content, layer }
    }

    /// The window's content.
    #[cfg(test)]
    pub fn content(&self) -> &AnyView {
        &self.content
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(
                self.content
                    .clone()
                    .cached(StyleRefinement::default().size_full()),
            )
            .child(self.layer.clone())
    }
}

/// Paints the moving parts of the marks the content registered.
pub struct LiveLayer {
    layer: Rc<Layer>,
    _subscriptions: [Subscription; 3],
}

impl LiveLayer {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        init(cx);
        let layer = Rc::new(Layer::default());
        let id = window.window_handle().window_id();
        cx.global_mut::<Motion>().layers.insert(id, layer.clone());
        Self {
            layer,
            _subscriptions: [
                cx.on_release(move |_, cx| {
                    if cx.has_global::<Motion>() {
                        cx.global_mut::<Motion>().layers.remove(&id);
                    }
                }),
                // Frames stop while the window is hidden; paint again when
                // it shows.
                cx.observe_window_visibility(window, |_, visibility, _, cx| {
                    if visibility.is_visible() {
                        cx.notify();
                    }
                }),
                // macOS posts no change of "Reduce motion"; read it again on
                // coming back to the window.
                cx.observe_window_activation(window, |_, window, cx| {
                    if window.is_window_active() {
                        gpui_kit::base::apply_system_reduce_motion(cx);
                    }
                }),
            ],
        }
    }
}

impl Render for LiveLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let layer = self.layer.clone();
        let this = cx.weak_entity();
        canvas(
            |_, _, _| {},
            move |_, _, window, cx| paint_layer(&layer, &this, window, cx),
        )
        .absolute()
        .size_full()
    }
}

fn paint_layer(layer: &Rc<Layer>, this: &WeakEntity<LiveLayer>, window: &mut Window, cx: &mut App) {
    layer.count(|stats| stats.layer_paints += 1);
    let Some(motion) = cx.try_global::<Motion>() else {
        return;
    };
    let fixed = motion.fixed;
    let elapsed =
        fixed.unwrap_or_else(|| cx.background_executor().now().duration_since(motion.epoch));
    let mut moved = 0;
    if !cx.reduce_motion() && !layer.still.get() {
        let registry = layer.registry.borrow();
        for (mark, clip, order) in &registry.marks {
            let occluders = registry
                .occluders
                .iter()
                .filter(|(_, over)| over > order)
                .map(|(bounds, _)| *bounds);
            let Some(clip) = visible(mark.extent().intersect(clip), occluders) else {
                continue;
            };
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                mark.paint_at(elapsed, window, cx)
            });
            moved += 1;
        }
    }
    log_motion(layer, moved, elapsed);
    if moved == 0 || fixed.is_some() || !window.is_visible() || layer.pending.get() {
        return;
    }
    layer.pending.set(true);
    layer.count(|stats| stats.frames += 1);
    let pending = layer.clone();
    let this = this.clone();
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(FRAME).await;
            pending.pending.set(false);
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
}

/// The part of `clip` not under `occluders`, as one rectangle: exact when an
/// occluder spans the clip in one direction (a card's edge across a row),
/// otherwise the largest uncovered piece; never any of an occluder.
fn visible(
    clip: Bounds<Pixels>,
    occluders: impl Iterator<Item = Bounds<Pixels>>,
) -> Option<Bounds<Pixels>> {
    let mut clip = clip;
    for over in occluders {
        if clip.size.width <= px(0.) || clip.size.height <= px(0.) {
            return None;
        }
        if !clip.intersects(&over) {
            continue;
        }
        let pieces = [
            Bounds::from_corners(clip.origin, point(clip.right(), over.top())),
            Bounds::from_corners(point(clip.left(), over.bottom()), clip.bottom_right()),
            Bounds::from_corners(clip.origin, point(over.left(), clip.bottom())),
            Bounds::from_corners(point(over.right(), clip.top()), clip.bottom_right()),
        ];
        clip = pieces
            .into_iter()
            .filter(|piece| piece.size.width > px(0.) && piece.size.height > px(0.))
            .max_by(|a, b| area(a).total_cmp(&area(b)))?;
    }
    (clip.size.width > px(0.) && clip.size.height > px(0.)).then_some(clip)
}

fn area(bounds: &Bounds<Pixels>) -> f32 {
    f32::from(bounds.size.width) * f32::from(bounds.size.height)
}

/// Under `GITEZ_DEBUG_THREADS`: when marks start and stop moving, and the
/// frames painted each second in between.
fn log_motion(layer: &Layer, moved: usize, elapsed: Duration) {
    if !crate::agent_threads::debugging() {
        return;
    }
    let frames = layer.stats.get().frames;
    match (layer.moving.replace(moved > 0), moved > 0) {
        (false, true) => {
            crate::agent_threads::debug(|| format!("motion: {moved} marks move"));
            layer.second.set(Some((elapsed, frames)));
        }
        (true, false) => {
            crate::agent_threads::debug(|| format!("motion: still, {frames} frames in all"));
            layer.second.set(None);
        }
        (true, true) => {
            if let Some((since, from)) = layer.second.get()
                && elapsed.saturating_sub(since) >= Duration::from_secs(1)
            {
                crate::agent_threads::debug(|| {
                    format!(
                        "motion: {} frames in the last {:.1} s, {moved} marks",
                        frames - from,
                        elapsed.saturating_sub(since).as_secs_f32()
                    )
                });
                layer.second.set(Some((elapsed, frames)));
            }
        }
        (false, false) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::{Bounds, point, px, size};

    use super::{blink, cubic_bezier, pulse, spin, visible};

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn css_curves_match_their_end_points_and_shape() {
        let ease_out = (0., 0., 0.58, 1.);
        assert!(close(cubic_bezier(ease_out, 0.), 0.));
        assert!(close(cubic_bezier(ease_out, 1.), 1.));
        // Ease-out is ahead of linear at the start.
        assert!(cubic_bezier(ease_out, 0.25) > 0.35);
        let ease_in_out = (0.42, 0., 0.58, 1.);
        assert!(close(cubic_bezier(ease_in_out, 0.5), 0.5));
        assert!(cubic_bezier(ease_in_out, 0.1) < 0.05);
    }

    #[test]
    fn the_pulse_grows_from_three_quarters_and_fades_out() {
        let (scale, opacity) = pulse(ms(0));
        assert!(close(scale, 0.75) && close(opacity, 0.9));
        let (scale, opacity) = pulse(ms(1799));
        assert!(close(scale, 1.7) && opacity < 0.01);
        let (middle, _) = pulse(ms(900));
        assert!(middle > 1.225, "eased out: past half way at half time");
        // It repeats.
        assert_eq!(pulse(ms(300)), pulse(ms(2100)));
    }

    #[test]
    fn the_spin_turns_once_in_three_seconds_and_a_fifth() {
        assert!(close(spin(ms(0)), 0.));
        assert!(close(spin(ms(800)), std::f32::consts::FRAC_PI_2));
        assert!(close(spin(ms(3200)), 0.));
    }

    #[test]
    fn the_blink_dims_to_thirty_five_percent_at_half_time() {
        assert!(close(blink(ms(0)), 1.));
        assert!(close(blink(ms(800)), 0.35));
        assert!(close(blink(ms(1600)), 1.));
        assert!(blink(ms(400)) < 1. && blink(ms(400)) > 0.35);
    }

    #[test]
    fn marks_are_clipped_away_from_occluders() {
        let clip = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(20.), px(20.)),
        };
        // A card over the lower half, wider than the mark: the top half.
        let card = Bounds {
            origin: point(px(-100.), px(12.)),
            size: size(px(400.), px(100.)),
        };
        assert_eq!(
            visible(clip, [card].into_iter()),
            Some(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(20.), px(12.)),
            })
        );
        // Covered entirely: nothing.
        let cover = Bounds {
            origin: point(px(-1.), px(-1.)),
            size: size(px(30.), px(30.)),
        };
        assert_eq!(visible(clip, [cover].into_iter()), None);
        // Elsewhere: all of it.
        let away = Bounds {
            origin: point(px(50.), px(50.)),
            size: size(px(10.), px(10.)),
        };
        assert_eq!(visible(clip, [away].into_iter()), Some(clip));
    }
}
