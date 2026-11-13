//! Live marks seen moving: the history's live lanes drawn at several
//! points of the test clock, to check that the pixels change (and stay put
//! with reduced motion), saved as `anim-*.png`. Like every screenshot test
//! they return at once unless `GITEZ_SCREENSHOTS` is set;
//! `screenshot_anim_measure` also needs `GITEZ_MEASURE=1`.

use std::path::Path;
use std::time::Duration;

use gitez_agent_thread::State;
use gpui_kit::{AnyWindowHandle, Bounds, Pixels, Point, point, px, size};
use image::RgbaImage;

use super::motion_tests::{history, index_and_settings, live_thread};
use crate::motion::{self, BLINK_PERIOD, Mark, PULSE_PERIOD, SPIN_PERIOD};
use crate::screenshot::Shots;

/// A repository of `commits` commits with a running and a preparing
/// thread, open in the main window, its marks moving with the test clock.
fn open_lanes(dir: &Path, commits: usize) -> (Shots, AnyWindowHandle) {
    let work_dir = history(dir, commits);
    let data_dir = dir.join("data");
    index_and_settings(&work_dir, &data_dir);
    live_thread(
        &work_dir,
        &data_dir,
        "Add a blame view to the commit detail",
        State::Running,
    );
    live_thread(
        &work_dir,
        &data_dir,
        "Keyboard navigation in the conflict editor",
        State::Preparing,
    );
    let mut shots = Shots::from_env(&data_dir).expect("enabled");
    let (window, _) = shots.open_workspace(&work_dir);
    shots.set_motion_time(None);
    shots.settle();
    (shots, window)
}

fn marks(shots: &mut Shots, window: AnyWindowHandle) -> Vec<Mark> {
    shots.update_window(window, |window, cx| {
        gpui_kit::test::TestWindowExt::render_frame(window, cx);
        motion::marks(window, cx)
    })
}

fn around(center: Point<Pixels>, half: f32) -> Bounds<Pixels> {
    Bounds {
        origin: point(center.x - px(half), center.y - px(half)),
        size: size(px(half * 2.), px(half * 2.)),
    }
}

/// Pixels that differ between `a` and `b` by more than a little.
fn changed(a: &RgbaImage, b: &RgbaImage) -> usize {
    a.pixels()
        .zip(b.pixels())
        .filter(|(a, b)| a.0.iter().zip(b.0).any(|(a, b)| a.abs_diff(b) > 8))
        .count()
}

/// `frames` side by side, each enlarged 4×, to see the motion at a glance.
fn strip(frames: &[RgbaImage]) -> RgbaImage {
    let (width, height) = (frames[0].width() * 4, frames[0].height() * 4);
    let mut strip = RgbaImage::from_pixel(
        (width + 8) * frames.len() as u32,
        height,
        image::Rgba([60, 60, 66, 255]),
    );
    for (index, frame) in frames.iter().enumerate() {
        let large =
            image::imageops::resize(frame, width, height, image::imageops::FilterType::Nearest);
        image::imageops::replace(&mut strip, &large, ((width + 8) * index as u32).into(), 0);
    }
    strip
}

/// `count` captures of `bounds`, `period / count` apart on the test clock.
fn sequence(
    shots: &mut Shots,
    window: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    period: Duration,
    count: u32,
) -> Vec<RgbaImage> {
    (0..count)
        .map(|index| {
            if index > 0 {
                shots.advance(period / count);
            }
            shots.capture_region(window, bounds)
        })
        .collect()
}

/// The running lane's pulse grows and fades, the preparing lane's dashed
/// ring turns and the chips' dots blink: six frames across each period
/// differ where the mark is. With reduced motion the same frames are the
/// same, and a commit's node never changes.
#[test]
fn screenshot_anim_live_lanes() {
    if !crate::screenshot::enabled() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let (mut shots, window) = open_lanes(dir.path(), 40);
    let marks = marks(&mut shots, window);
    let pulse = marks
        .iter()
        .find_map(|mark| match mark {
            Mark::Pulse { center, .. } => Some(*center),
            _ => None,
        })
        .expect("the running lane pulses");
    let spin = marks
        .iter()
        .find_map(|mark| match mark {
            Mark::Spin { center, .. } => Some(*center),
            _ => None,
        })
        .expect("the preparing lane spins");
    let blink = marks
        .iter()
        .find_map(|mark| match mark {
            Mark::Blink { center, .. } if (center.y - pulse.y).abs() < px(10.) => Some(*center),
            _ => None,
        })
        .expect("the running chip's dot blinks");
    shots.save(window, "anim-lanes");

    let pulse_box = around(pulse, 18.);
    let spin_box = around(spin, 10.);
    let blink_box = around(blink, 6.);
    // Below the lanes: the first commit's node, which never moves.
    let still_box = around(point(pulse.x, pulse.y + px(36. * 3.)), 14.);

    let pulses = sequence(&mut shots, window, pulse_box, PULSE_PERIOD, 6);
    let spins = sequence(&mut shots, window, spin_box, SPIN_PERIOD, 6);
    let blinks = sequence(&mut shots, window, blink_box, BLINK_PERIOD, 4);
    for (name, frames) in [("pulse", &pulses), ("spin", &spins), ("blink", &blinks)] {
        for (index, frame) in frames.iter().enumerate() {
            let large = image::imageops::resize(
                frame,
                frame.width() * 4,
                frame.height() * 4,
                image::imageops::FilterType::Nearest,
            );
            shots.save_image(large, &format!("anim-{name}-{index}"));
        }
        shots.save_image(strip(frames), &format!("anim-{name}"));
        for pair in frames.windows(2) {
            let changed = changed(&pair[0], &pair[1]);
            assert!(
                changed > 6,
                "{name}: consecutive frames differ ({changed} px)"
            );
        }
    }
    let first = shots.capture_region(window, still_box);
    shots.advance(Duration::from_millis(700));
    let later = shots.capture_region(window, still_box);
    assert_eq!(changed(&first, &later), 0, "a commit's node stands still");

    shots.update(|cx| cx.set_reduce_motion(true));
    for (name, bounds, period) in [
        ("pulse", pulse_box, PULSE_PERIOD),
        ("spin", spin_box, SPIN_PERIOD),
        ("blink", blink_box, BLINK_PERIOD),
    ] {
        let frames = sequence(&mut shots, window, bounds, period, 3);
        for pair in frames.windows(2) {
            assert_eq!(
                changed(&pair[0], &pair[1]),
                0,
                "{name} stands still with reduced motion"
            );
        }
        shots.save_image(strip(&frames), &format!("anim-{name}-reduced"));
    }
}

/// Times a frame of a window on 50,000 commits with two live lanes, with
/// the macOS text system: the live layer alone, the history rendered again
/// (what an animation inside it would cost), the whole window refreshed.
#[test]
fn screenshot_anim_measure() {
    if !crate::screenshot::enabled() || std::env::var_os("GITEZ_MEASURE").is_none() {
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let (mut shots, window) = open_lanes(dir.path(), 50_000);
    let workspace = shots.update_window(window, |window, _| {
        window
            .root::<gpui_kit::base::Root>()
            .flatten()
            .expect("a root")
    });
    let history = shots.update(|cx| {
        let shell = workspace.read(cx).view().clone();
        shell
            .downcast::<motion::Shell>()
            .ok()
            .and_then(|shell| {
                shell
                    .read(cx)
                    .content()
                    .clone()
                    .downcast::<super::Workspace>()
                    .ok()
            })
            .map(|workspace| workspace.read(cx).history.clone())
            .expect("the workspace")
    });
    let commits = shots.update(|cx| history.read(cx).commits().len());
    eprintln!("history: {commits} commits");
    let frames = 120;
    shots.time_frames(window, frames, |_, _| {});
    // To profile the layer's frames: `GITEZ_MEASURE_FRAMES=5000`.
    if let Some(many) = std::env::var("GITEZ_MEASURE_FRAMES")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        eprintln!("profiling {many} frames, pid {}", std::process::id());
        shots.time_frames(window, many, |_, _| {});
    }
    let layer = shots.time_frames(window, frames, |_, _| {});
    let history_frame = shots.time_frames(window, frames, |_, cx| {
        history.update(cx, |_, cx| cx.notify())
    });
    let refresh = shots.time_frames(window, frames, |window, _| window.refresh());
    eprintln!("a frame, live layer only:              {layer:?}");
    eprintln!("a frame, the history rendered again:   {history_frame:?}");
    eprintln!("a frame, the whole window refreshed:   {refresh:?}");
}
