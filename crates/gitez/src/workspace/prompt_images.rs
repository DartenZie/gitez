//! Images attached to a prompt for an agent: pasted (`⌘V`) or dropped into
//! the "Ask an agent" composer (graph and Home) or a thread's reply box,
//! shown there as a row of thumbnails, and sent with the message.
//!
//! [`PromptImages`] holds a prompt's images while they are made fit to send
//! ([`gitez_agent_thread::prepare_image`], in the background) and once they
//! are ready. Whether the agent's model takes images at all is asked of the
//! agent ([`gitez_agent::Agent::accepts_images`]) by whoever shows the
//! prompt, and passed in as [`Accepts`]. [`image_tile`] draws one
//! thumbnail, here and in the thread view.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gitez_agent_thread::{
    ImageError, MAX_IMAGES, MAX_INPUT_BYTES, PreparedImage, Thumbnail, size_text,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::*;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::color;

/// The side of a thumbnail tile.
pub const TILE: Pixels = px(52.);
/// The short side of a thumbnail's pixels: twice the tile, for Retina.
pub const THUMBNAIL_PIXELS: u32 = 104;
/// Extensions of files that may be images, for a paste of copied files.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp"];

/// Whether the prompt's agent and model take images: `Err` says why not,
/// for the notice a paste of an image shows.
pub type Accepts = Result<(), SharedString>;

/// What [`Accepts`] is for an agent that is `agent` (its name) with
/// `model`, as [`gitez_agent::Agent::accepts_images`] answers.
pub fn accepts(agent: Option<&dyn gitez_agent::Agent>, model: Option<&str>) -> Accepts {
    match agent {
        Some(agent) if agent.accepts_images(model) => Ok(()),
        Some(agent) => Err(match model.filter(|model| !model.is_empty()) {
            Some(model) => format!(
                "{} with the model {model} does not take images.",
                agent.name()
            ),
            None => format!("{} does not take images.", agent.name()),
        }
        .into()),
        None => Err("This agent does not take images.".into()),
    }
}

/// One attached image.
struct Item {
    id: u64,
    state: ItemState,
}

enum ItemState {
    /// Being read, converted or scaled down in the background.
    Preparing(#[allow(dead_code)] Task<()>),
    Ready {
        image: Arc<PreparedImage>,
        thumbnail: Arc<RenderImage>,
    },
}

/// The images attached to one prompt.
pub struct PromptImages {
    /// For element ids: `<name>-image-<n>`.
    name: &'static str,
    items: Vec<Item>,
    next_id: u64,
}

impl PromptImages {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            items: Vec::new(),
            next_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether an image is still being prepared: the message waits.
    pub fn preparing(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item.state, ItemState::Preparing(_)))
    }

    /// Takes a paste with images: an image on the clipboard, or copied
    /// files of which some are images. Returns whether it took the paste;
    /// `false` lets the prompt insert the clipboard's text as before. When
    /// the agent does not take images (`accepts`), nothing is attached and
    /// a notice says why; the text, if any, is still pasted.
    pub fn paste(
        &mut self,
        item: &ClipboardItem,
        accepts: &Accepts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut images = Vec::new();
        let mut paths = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => images.push(image.bytes.clone()),
                ClipboardEntry::ExternalPaths(external) => paths.extend(
                    external
                        .paths()
                        .iter()
                        .filter(|path| may_be_image(path))
                        .cloned(),
                ),
                ClipboardEntry::String(_) => {}
            }
        }
        if images.is_empty() && paths.is_empty() {
            return false;
        }
        if let Err(why) = accepts {
            window.push_notification(crate::toast::warning(why.clone()), cx);
            return item.text().is_none_or(|text| text.is_empty());
        }
        for bytes in images {
            self.add(Source::Bytes(bytes), window, cx);
        }
        for path in paths {
            self.add(Source::File(path), window, cx);
        }
        true
    }

    /// Attaches the image files among `paths` (a drop of files).
    pub fn drop_paths(
        &mut self,
        paths: &[PathBuf],
        accepts: &Accepts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let images: Vec<&PathBuf> = paths.iter().filter(|path| may_be_image(path)).collect();
        if images.is_empty() {
            window.push_notification(
                crate::toast::warning("Only images can be given to the agent."),
                cx,
            );
            return;
        }
        if let Err(why) = accepts {
            window.push_notification(crate::toast::warning(why.clone()), cx);
            return;
        }
        for path in images {
            self.add(Source::File(path.clone()), window, cx);
        }
    }

    /// Prepares `source` in the background and adds it, unless the message
    /// has [`MAX_IMAGES`] already.
    fn add(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        if self.items.len() >= MAX_IMAGES {
            window.push_notification(
                crate::toast::warning(format!(
                    "A message can have at most {MAX_IMAGES} images; remove one to add another."
                )),
                cx,
            );
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let prepared = cx
            .background_executor()
            .spawn(async move { prepare(source) });
        let task = cx.spawn_in(window, async move |this, cx| {
            let prepared = prepared.await;
            this.update_in(cx, |this, window, cx| {
                let Some(index) = this.items.iter().position(|item| item.id == id) else {
                    return;
                };
                match prepared {
                    Ok((image, thumbnail)) => {
                        this.items[index].state = ItemState::Ready {
                            image: Arc::new(image),
                            thumbnail: render_image(thumbnail),
                        };
                    }
                    Err(why) => {
                        this.items.remove(index);
                        window.push_notification(crate::toast::error(why), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        self.items.push(Item {
            id,
            state: ItemState::Preparing(task),
        });
        cx.notify();
    }

    /// Adds images already prepared, as after a message that could not be
    /// sent; their thumbnails are made in the background.
    pub fn restore(
        &mut self,
        images: Vec<Arc<PreparedImage>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for image in images {
            if self.items.len() >= MAX_IMAGES {
                break;
            }
            let id = self.next_id;
            self.next_id += 1;
            let bytes = image.clone();
            let thumbnail = cx.background_executor().spawn(async move {
                gitez_agent_thread::thumbnail(&bytes.bytes, THUMBNAIL_PIXELS)
            });
            let task = cx.spawn_in(window, async move |this, cx| {
                let thumbnail = thumbnail.await;
                this.update(cx, |this, cx| {
                    if let Some(item) = this.items.iter_mut().find(|item| item.id == id) {
                        match thumbnail {
                            Ok(thumbnail) => {
                                item.state = ItemState::Ready {
                                    image,
                                    thumbnail: render_image(thumbnail),
                                }
                            }
                            Err(_) => this.items.retain(|item| item.id != id),
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
            self.items.push(Item {
                id,
                state: ItemState::Preparing(task),
            });
        }
        cx.notify();
    }

    /// The images ready to send, in order, leaving the prompt without
    /// images. Call only when none is [`preparing`](Self::preparing).
    pub fn take(&mut self, cx: &mut Context<Self>) -> Vec<Arc<PreparedImage>> {
        let items = std::mem::take(&mut self.items);
        cx.notify();
        items
            .into_iter()
            .filter_map(|item| match item.state {
                ItemState::Ready { image, thumbnail } => {
                    cx.drop_image(thumbnail, None);
                    Some(image)
                }
                ItemState::Preparing(_) => None,
            })
            .collect()
    }

    /// The images ready to send, in order; they stay attached until
    /// [`clear`](Self::clear).
    pub fn ready(&self) -> Vec<Arc<PreparedImage>> {
        self.items
            .iter()
            .filter_map(|item| match &item.state {
                ItemState::Ready { image, .. } => Some(image.clone()),
                ItemState::Preparing(_) => None,
            })
            .collect()
    }

    /// Removes every image, as after its message was sent.
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for item in std::mem::take(&mut self.items) {
            if let ItemState::Ready { thumbnail, .. } = item.state {
                cx.drop_image(thumbnail, Some(window));
            }
        }
        cx.notify();
    }

    /// Removes image `id`.
    fn remove(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.items.iter().position(|item| item.id == id) {
            let item = self.items.remove(index);
            if let ItemState::Ready { thumbnail, .. } = item.state {
                cx.drop_image(thumbnail, Some(window));
            }
            cx.notify();
        }
    }

    /// The row of thumbnails, if there are images: each with a remove
    /// control that the keyboard reaches with Tab.
    pub fn render(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.items.is_empty() {
            return None;
        }
        let name = self.name;
        let tiles = self.items.iter().enumerate().map(|(index, item)| {
            let id = item.id;
            let remove =
                cx.listener(move |this, _: &ClickEvent, window, cx| this.remove(id, window, cx));
            let element_id = ElementId::Name(format!("{name}-image-{index}").into());
            match &item.state {
                ItemState::Ready { image, thumbnail } => image_tile(
                    element_id,
                    Some(thumbnail.clone()),
                    describe(image),
                    Some(Box::new(remove)),
                ),
                ItemState::Preparing(_) => image_tile(
                    element_id,
                    None,
                    "Preparing the image…".into(),
                    Some(Box::new(remove)),
                ),
            }
        });
        Some(
            div()
                .debug_selector(move || format!("{name}-images"))
                .h_flex()
                .flex_wrap()
                .gap(px(8.))
                .children(tiles),
        )
    }
}

/// "screenshot 1 · PNG · 1280 × 800 · 340 KB, scaled down from 5120 × 3200".
fn describe(image: &PreparedImage) -> SharedString {
    let mut text = format!(
        "{} · {} × {} · {}",
        image.media_type.label(),
        image.width,
        image.height,
        size_text(image.bytes.len() as u64)
    );
    if let Some(changed) = image.changed {
        if (changed.from_width, changed.from_height) != (image.width, image.height) {
            text.push_str(&format!(
                ", scaled down from {} × {}",
                changed.from_width, changed.from_height
            ));
        }
        if changed.from_format != image.media_type.label() {
            text.push_str(&format!(", converted from {}", changed.from_format));
        }
    }
    text.into()
}

/// What an image is attached from.
enum Source {
    /// The clipboard's bytes.
    Bytes(Vec<u8>),
    /// A copied or dropped file.
    File(PathBuf),
}

/// `source` made fit to send, and its thumbnail; or why not, for the user.
fn prepare(source: Source) -> Result<(PreparedImage, Thumbnail), String> {
    let (bytes, name) = match source {
        Source::Bytes(bytes) => (bytes, None),
        Source::File(path) => {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let metadata = std::fs::metadata(&path)
                .map_err(|error| format!("GitEZ could not read {name}: {error}"))?;
            if !metadata.is_file() {
                return Err(format!("{name} is not a file."));
            }
            if metadata.len() > MAX_INPUT_BYTES {
                return Err(format!(
                    "{name} is too large to attach: {}, at most {}.",
                    size_text(metadata.len()),
                    size_text(MAX_INPUT_BYTES)
                ));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("GitEZ could not read {name}: {error}"))?;
            (bytes, Some(name))
        }
    };
    let said = |error: ImageError| match &name {
        Some(name) => format!("{name}: {error}"),
        None => error.to_string(),
    };
    let image = gitez_agent_thread::prepare_image(&bytes).map_err(said)?;
    let thumbnail = gitez_agent_thread::thumbnail(&image.bytes, THUMBNAIL_PIXELS).map_err(said)?;
    Ok((image, thumbnail))
}

/// Whether `path` is named like an image GitEZ reads. The bytes decide in
/// the end; this only keeps a paste of other files a paste of their names.
fn may_be_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// A thumbnail GPUI can draw.
pub fn render_image(thumbnail: Thumbnail) -> Arc<RenderImage> {
    let Thumbnail {
        width,
        height,
        bgra,
    } = thumbnail;
    let buffer = image::RgbaImage::from_raw(width, height, bgra)
        .unwrap_or_else(|| image::RgbaImage::new(1, 1));
    Arc::new(RenderImage::new([image::Frame::new(buffer)]))
}

/// What happens when a tile's remove control is used.
pub type OnRemove = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A square thumbnail `id`: `thumbnail` covering it (a spinner while there
/// is none), `tooltip` on hover, and a remove control in its corner when
/// `on_remove` is given.
pub fn image_tile(
    id: ElementId,
    thumbnail: Option<Arc<RenderImage>>,
    tooltip: SharedString,
    on_remove: Option<OnRemove>,
) -> Div {
    let remove_id = ElementId::Name(format!("{id}-remove").into());
    let tile = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(TILE)
        .rounded(px(6.))
        .overflow_hidden()
        .border_1()
        .border_color(rgb(color::BUTTON_BORDER))
        .bg(rgb(color::BUTTON))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .map(|this| match thumbnail {
            Some(thumbnail) => this.child(img(thumbnail).size_full().object_fit(ObjectFit::Cover)),
            None => this.child(crate::motion::spinning_ring(
                px(16.),
                px(2.),
                color::TEXT_MUTED,
            )),
        });
    div()
        .relative()
        .flex_none()
        .child(tile)
        .when_some(on_remove, |this, on_remove| {
            // A disc in the panel's colour over the tile's corner, so the
            // control shows on any image.
            this.child(
                div()
                    .absolute()
                    .top(px(-6.))
                    .right(px(-6.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(color::BUTTON_BORDER))
                    .bg(rgb(color::PANEL))
                    .child(
                        Button::new(remove_id)
                            .ghost()
                            .xsmall()
                            .size(px(18.))
                            .rounded_full()
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .icon(Icon::new(IconName::Close).size(px(10.)))
                            .accessibility_label("Remove the image")
                            .tooltip("Remove the image")
                            .on_click(move |event, window, cx| on_remove(event, window, cx)),
                    ),
            )
        })
}
