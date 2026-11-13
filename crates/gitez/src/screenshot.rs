//! Screenshots of real views, for agents that cannot see the screen.
//!
//! [`Shots`] runs GitEZ's views on GPUI's test platform, but with the macOS
//! text system (Core Text and the installed fonts) and the Metal renderer,
//! so a PNG shows what the app draws: the same fonts, theme, icons and
//! layout, at the Retina scale (2×). No window appears on screen and no
//! screen-recording permission is needed. Time and tasks are those of the
//! test platform: they advance only in [`Shots::settle`].
//!
//! Screenshot tests return at once unless `GITEZ_SCREENSHOTS` names the
//! folder to write the PNGs to, so `cargo test` skips them:
//!
//! ```sh
//! GITEZ_SCREENSHOTS=/path/to/shots cargo test -p gitez screenshot_
//! ```

#![allow(
    dead_code,
    reason = "a toolkit for screenshot tests; each uses only some of it"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::*;

use crate::agent_threads::{Options, init_with};
use crate::workspace::Workspace;

/// The environment variable naming the folder the PNGs are written to.
pub const DIR_ENV: &str = "GITEZ_SCREENSHOTS";
/// Optional: a folder of `.ttf` and `.otf` files to load before the theme
/// picks its fonts, to see GitEZ with Geist and JetBrains Mono on a machine
/// where they are not installed. The app itself uses installed fonts only.
pub const FONTS_ENV: &str = "GITEZ_SCREENSHOT_FONTS";

/// Whether screenshot tests run: `GITEZ_SCREENSHOTS` names a folder.
pub fn enabled() -> bool {
    std::env::var_os(DIR_ENV).is_some_and(|dir| !dir.is_empty())
}

/// The size of the main window when GitEZ opens.
pub fn window_size() -> Size<Pixels> {
    size(px(1440.), px(900.))
}

/// How many steps of 50 ms of test time [`Shots::settle`] runs.
const SETTLE_STEPS: usize = 40;

/// A GitEZ app on the test platform that draws real pixels.
pub struct Shots {
    cx: HeadlessAppContext,
    dir: PathBuf,
}

impl Shots {
    /// The app, with the GitEZ theme and key bindings, and agent threads
    /// kept in `data_dir`; `None` unless `GITEZ_SCREENSHOTS` is set.
    ///
    /// The recent repositories are kept in memory: the user's own data
    /// folder is never read or written.
    pub fn from_env(data_dir: &Path) -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os(DIR_ENV).filter(|dir| !dir.is_empty())?);
        std::fs::create_dir_all(&dir).expect("the screenshot folder can be created");
        let mut cx = HeadlessAppContext::with_platform(
            Arc::new(gpui_apple::AppleTextSystem::new()),
            Arc::new(crate::assets::Assets),
            gpui_kit::platform::current_headless_renderer,
        );
        let data_dir = data_dir.to_owned();
        cx.update(|cx| {
            gpui_kit::init(cx);
            load_fonts(cx);
            crate::theme::init(cx);
            crate::recent::init_in_memory(cx);
            init_with(
                Options {
                    data_dir: Some(data_dir),
                    sandbox: Arc::new(gitez_sandbox::Passthrough),
                    agents: gitez_agent::Registry::builtin(),
                    secrets: Arc::new(gitez_secret::Memory::default()),
                    exe: Err("no supervisor in screenshots".into()),
                    watch: false,
                },
                cx,
            );
            cx.bind_keys(crate::workspace::key_bindings());
        });
        let mut shots = Self { cx, dir };
        shots.settle();
        Some(shots)
    }

    /// Opens a window of `size` with the view `build` returns, inside
    /// gpui-kit's `Root` as in the app, and lets it load.
    pub fn open<V: Render + 'static>(
        &mut self,
        size: Size<Pixels>,
        build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
    ) -> (AnyWindowHandle, Entity<V>) {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size,
            })),
            focus: true,
            show: true,
            ..Default::default()
        };
        let opened = self
            .cx
            .update(|cx| gpui_kit::open_window(options, cx, build))
            .expect("the window opens");
        self.settle();
        opened
    }

    /// The main window at its opening size, with the repository at `path`.
    pub fn open_workspace(&mut self, path: &Path) -> (AnyWindowHandle, Entity<Workspace>) {
        let path = path.to_owned();
        self.open(window_size(), |window, cx| {
            cx.new(|cx| Workspace::new(Some(path), window, cx))
        })
    }

    /// Runs what is pending and two seconds of timers, so repositories,
    /// transcripts and icons load and debounced work happens.
    pub fn settle(&mut self) {
        for _ in 0..SETTLE_STEPS {
            self.cx.run_until_parked();
            self.cx.advance_clock(Duration::from_millis(50));
        }
        self.cx.run_until_parked();
    }

    /// Runs `f` on the app, for example to update a view's entity.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut App) -> R) -> R {
        let result = self.cx.update(f);
        self.settle();
        result
    }

    /// Runs `f` on `window`.
    pub fn update_window<R>(
        &mut self,
        window: AnyWindowHandle,
        f: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        let result = self
            .cx
            .update_window(window, |_, window, cx| f(window, cx))
            .expect("the window is open");
        self.settle();
        result
    }

    /// Dispatches `action` to the focused element of `window`, as a menu
    /// item or key binding would.
    pub fn dispatch(&mut self, window: AnyWindowHandle, action: impl Action) {
        self.update_window(window, |window, cx| {
            window.dispatch_action(action.boxed_clone(), cx)
        });
    }

    /// Presses keys, separated by spaces: `"cmd-k"`, `"down down enter"`.
    /// Key bindings run as in the app.
    pub fn keys(&mut self, window: AnyWindowHandle, keys: &str) {
        for key in keys.split(' ').filter(|key| !key.is_empty()) {
            self.update_window(window, |window, cx| window.press(key, cx));
        }
    }

    /// Types `text` into the focused text field.
    pub fn type_text(&mut self, window: AnyWindowHandle, text: &str) {
        self.update_window(window, |window, cx| window.input(text, cx));
    }

    /// Moves the pointer to `position` (window coordinates, logical pixels),
    /// so hover styles and tooltips show.
    pub fn hover(&mut self, window: AnyWindowHandle, position: Point<Pixels>) {
        self.update_window(window, |window, cx| {
            window.render_frame(cx);
            let modifiers = window.modifiers();
            window.dispatch_event(
                MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                }
                .to_platform_input(),
                cx,
            );
        });
    }

    /// Clicks the left button at `position` (window coordinates, logical
    /// pixels).
    pub fn click(&mut self, window: AnyWindowHandle, position: Point<Pixels>) {
        self.hover(window, position);
        self.update_window(window, |window, cx| {
            let modifiers = window.modifiers();
            window.dispatch_event(
                MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }
                .to_platform_input(),
                cx,
            );
            window.render_frame(cx);
            window.dispatch_event(
                MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count: 1,
                }
                .to_platform_input(),
                cx,
            );
        });
    }

    /// Scrolls by `lines` (positive: down) at `position`.
    pub fn scroll(&mut self, window: AnyWindowHandle, position: Point<Pixels>, lines: f32) {
        self.hover(window, position);
        self.update_window(window, |window, cx| {
            window.dispatch_event(
                ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Lines(point(0., -lines)),
                    modifiers: window.modifiers(),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        });
    }

    /// Draws `window` and writes it to `<GITEZ_SCREENSHOTS>/<name>.png`.
    pub fn save(&mut self, window: AnyWindowHandle, name: &str) -> PathBuf {
        let image = self.render(window);
        self.write(image, name)
    }

    /// Like [`Shots::save`], but only the part of the window in `bounds`
    /// (logical pixels), to look at a detail at full resolution.
    pub fn save_region(
        &mut self,
        window: AnyWindowHandle,
        bounds: Bounds<Pixels>,
        name: &str,
    ) -> PathBuf {
        let image = self.render(window);
        let scale = |value: Pixels| (f32::from(value) * 2.).round().max(0.) as u32;
        let x = scale(bounds.origin.x).min(image.width());
        let y = scale(bounds.origin.y).min(image.height());
        let width = scale(bounds.size.width).min(image.width() - x);
        let height = scale(bounds.size.height).min(image.height() - y);
        let crop = image::imageops::crop_imm(&image, x, y, width, height).to_image();
        self.write(crop, name)
    }

    fn render(&mut self, window: AnyWindowHandle) -> image::RgbaImage {
        self.settle();
        self.cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.render_to_image()
            })
            .expect("the window is open")
            .expect("the window renders")
    }

    fn write(&self, image: image::RgbaImage, name: &str) -> PathBuf {
        let path = self.dir.join(format!("{name}.png"));
        image.save(&path).expect("the PNG is written");
        eprintln!("screenshot: {}", path.display());
        path
    }
}

/// Loads the fonts in the folder `GITEZ_SCREENSHOT_FONTS` names, if set.
fn load_fonts(cx: &mut App) {
    let Some(dir) = std::env::var_os(FONTS_ENV).filter(|dir| !dir.is_empty()) else {
        return;
    };
    let fonts: Vec<_> = std::fs::read_dir(&dir)
        .expect("the fonts folder is readable")
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "ttf" || extension == "otf")
        })
        .map(|path| std::borrow::Cow::Owned(std::fs::read(path).expect("the font is readable")))
        .collect();
    cx.text_system().add_fonts(fonts).expect("the fonts load");
}

/// Runs `git` in `dir` with a fixed identity and no user configuration.
pub fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Lovelace")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada Lovelace")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

/// A small repository in `dir`: a few commits on `main`, two branches, a
/// tag and an uncommitted change.
pub fn sample_repository(dir: &Path) {
    std::fs::create_dir_all(dir).expect("the repository folder can be created");
    git(dir, &["init", "-q", "-b", "main"]);
    let commit = |file: &str, text: &str, message: &str| {
        std::fs::write(dir.join(file), text).expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-qm", message]);
    };
    commit("README.md", "# Sample\n", "Start the project");
    commit(
        "src.rs",
        "fn main() {}\n",
        "Add the entry point and a longer subject line to see how it is cut",
    );
    git(dir, &["tag", "v0.1.0"]);
    git(dir, &["checkout", "-qb", "feature/parser"]);
    commit("parser.rs", "pub fn parse() {}\n", "Add a parser");
    commit(
        "parser.rs",
        "pub fn parse() -> u8 { 1 }\n",
        "Return a value",
    );
    git(dir, &["checkout", "-q", "main"]);
    commit(
        "README.md",
        "# Sample\n\nA sample.\n",
        "Describe the sample",
    );
    git(dir, &["checkout", "-qb", "fix/typo"]);
    commit("README.md", "# Sample\n\nA sample!\n", "Fix a typo");
    git(dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("notes.txt"), "draft\n").expect("write");
}

#[cfg(test)]
mod tests {
    use super::{Shots, sample_repository};

    /// The main window with a small repository's history.
    #[test]
    fn screenshot_main_window() {
        let dir = tempfile::tempdir().expect("temp dir");
        let Some(mut shots) = Shots::from_env(&dir.path().join("data")) else {
            return;
        };
        let repository = dir.path().join("sample");
        sample_repository(&repository);
        let (window, _) = shots.open_workspace(&repository);
        shots.save(window, "main-window");
        // Key bindings run as in the app: ⌘K opens the command palette.
        shots.keys(window, "cmd-k");
        shots.type_text(window, "log");
        shots.save(window, "main-window-palette");
    }
}
