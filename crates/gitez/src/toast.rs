//! Toast notifications with the icon centred on the first line of the message.
//!
//! gpui-kit's typed notifications (`Notification::success` and the rest) pin
//! their icon at a fixed offset from the top of the toast, which does not line
//! up with the message once the theme's font size differs from gpui-kit's
//! default. These constructors put the icon in the message row instead: it
//! sits beside an empty run of the message's own text, so flex layout centres
//! it on the first line whatever the line height is.

use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex};
use gpui_kit::*;

/// A notification that something finished.
pub fn success(message: impl Into<SharedString>) -> Notification {
    with_icon(IconName::CircleCheck, |cx| cx.theme().success, message)
}

/// A neutral notification.
pub fn info(message: impl Into<SharedString>) -> Notification {
    with_icon(IconName::Info, |cx| cx.theme().info, message)
}

/// A notification that something needs the user's attention.
pub fn warning(message: impl Into<SharedString>) -> Notification {
    with_icon(IconName::TriangleAlert, |cx| cx.theme().warning, message)
}

/// A notification that something failed.
pub fn error(message: impl Into<SharedString>) -> Notification {
    with_icon(IconName::CircleX, |cx| cx.theme().danger, message)
}

fn with_icon(
    icon: IconName,
    color: fn(&App) -> Hsla,
    message: impl Into<SharedString>,
) -> Notification {
    let message = message.into();
    Notification::new().content(move |_, _, cx| {
        h_flex()
            .items_start()
            .gap_2()
            .text_sm()
            .child(
                // The zero-width space gives this box the height of one line
                // of the message, and the icon is centred within it.
                h_flex()
                    .flex_none()
                    .items_center()
                    .child(Icon::new(icon.clone()).text_color(color(cx)))
                    .child("\u{200B}"),
            )
            .child(div().flex_1().min_w_0().child(message.clone()))
            .into_any_element()
    })
}
