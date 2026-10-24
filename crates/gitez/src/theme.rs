//! GitEZ design tokens and their mapping onto the gpui-kit theme.
//!
//! Views take colours from [`color`] and sizes from this module instead of
//! writing hex literals. [`init`] projects the same tokens onto the gpui-kit
//! theme so that its widgets (buttons, lists, scrollbars, menus) match.

use gpui_kit::component::theme::{Theme, ThemeColor, ThemeMode};
use gpui_kit::*;

/// Colours of the dark theme, as `0xRRGGBB` for [`rgb`] or `0xRRGGBBAA` for
/// [`rgba`].
pub mod color {
    pub const BACKGROUND: u32 = 0x12_12_14;
    /// Toolbar, status bar and side panels.
    pub const PANEL: u32 = 0x16_16_18;
    /// The repository sidebar, a shade darker than the other panels.
    pub const SIDEBAR: u32 = 0x15_15_17;
    pub const ROW_HOVER: u32 = 0x19_19_1d;
    pub const NAV_HOVER: u32 = 0x1d_1d_21;
    pub const NAV_ACTIVE: u32 = 0x22_22_26;
    pub const SELECTED: u32 = 0x2a_24_18;

    pub const BORDER: u32 = 0x26_26_2b;
    /// Hairline between rows of a list.
    pub const ROW_BORDER: u32 = 0x1b_1b_1f;
    /// Vertical separator between toolbar groups.
    pub const SEPARATOR: u32 = 0x2a_2a_2f;

    pub const BUTTON: u32 = 0x1f_1f_23;
    pub const BUTTON_HOVER: u32 = 0x27_27_2c;
    pub const BUTTON_BORDER: u32 = 0x2e_2e_34;

    pub const TEXT: u32 = 0xe7_e5_e1;
    pub const TEXT_SECONDARY: u32 = 0xcf_cb_c0;
    pub const TEXT_MUTED: u32 = 0x9a_98_8f;
    /// Section headings and the quietest labels.
    pub const TEXT_FAINT: u32 = 0x8d_8b_83;

    pub const ACCENT: u32 = 0xe8_b0_4b;
    pub const ACCENT_HOVER: u32 = 0xf0_bf_63;
    pub const ON_ACCENT: u32 = 0x1a_14_08;

    pub const BRANCH_BLUE: u32 = 0x5f_a8_f5;
    pub const BRANCH_VIOLET: u32 = 0xc0_8c_f0;
    pub const BRANCH_TEAL: u32 = 0x4f_c6_b0;

    /// The merge dialog: an option under the pointer, the box of an
    /// option's icon (the suggested option's own), and its dividers.
    pub const MERGE_OPTION_HOVER: u32 = 0x26_26_2b;
    pub const MERGE_ICON: u32 = 0x23_23_27;
    pub const MERGE_ICON_PRIMARY: u32 = 0x3a_2f_18;
    pub const MERGE_DIVIDER: u32 = 0x2a_2a_2f;

    /// The conflict editor, with alpha: a conflict's lines from our side
    /// and from theirs, faint and when picked for the result, and lines of
    /// a resolved conflict.
    pub const CONFLICT_OURS: u32 = 0xe8_b0_4b_17;
    pub const CONFLICT_OURS_PICKED: u32 = 0xe8_b0_4b_33;
    pub const CONFLICT_THEIRS: u32 = 0x4f_c6_b0_17;
    pub const CONFLICT_THEIRS_PICKED: u32 = 0x4f_c6_b0_33;
    pub const CONFLICT_RESOLVED: u32 = 0x6c_c5_8a_14;
    /// The tags saying where a result line comes from: ours, theirs, both.
    pub const SOURCE_OURS: u32 = 0x3a_2f_18;
    pub const SOURCE_THEIRS: u32 = 0x17_34_30;
    pub const SOURCE_BOTH: u32 = 0x2a_2a_30;
    /// The box ticking a line, unticked.
    pub const PICK_BORDER: u32 = 0x55_55_5b;
    /// The strip under the result.
    pub const RESULT_FOOTER: u32 = 0x17_17_1a;

    /// Dragging a branch onto another in the history: the strip naming it,
    /// and with alpha, the commits it would bring in and the row of the
    /// branch it would land on.
    pub const MOVING: u32 = 0x1d_2a_28;
    pub const MOVING_TEXT: u32 = 0x7f_d8_c6;
    pub const INCOMING_ROW: u32 = 0x4f_c6_b0_12;
    pub const TARGET_ROW: u32 = 0xe8_b0_4b_17;

    /// Cherry-picking: the bar over the history while commits are picked,
    /// the chosen target branch's border and a target's hover, a picked
    /// commit in the panel, and the trial run's verdicts.
    pub const PICK_BAR_BORDER: u32 = 0x36_36_3c;
    pub const PICK_TARGET_BORDER: u32 = 0x5a_4a_26;
    pub const PICK_TARGET_HOVER: u32 = 0x22_22_26;
    pub const PICK_COMMIT: u32 = 0x1b_1b_1e;
    pub const PICK_COMMIT_BORDER: u32 = 0x29_29_2e;
    pub const TRIAL_CLEAN: u32 = 0x17_20_1a;
    pub const TRIAL_CLEAN_BORDER: u32 = 0x28_40_33;
    pub const TRIAL_CLEAN_TEXT: u32 = 0xb9_dc_c5;
    pub const TRIAL_CONFLICT: u32 = 0x22_1d_14;

    /// Rewriting history: a reworded commit's card, a commit's chosen
    /// action (reword, fixup and drop have their own colours), the chip
    /// on a commit the upstream has, and the warning about pushed commits.
    pub const REWORD_CARD: u32 = 0x1a_18_14;
    pub const ACTION_ON: u32 = 0x2a_2a_30;
    pub const REWORD_ON: u32 = 0x3a_2f_18;
    pub const FIXUP_ON: u32 = 0x1e_2c_3d;
    pub const FIXUP_TEXT: u32 = 0x8f_c2_f8;
    pub const DROP_ON: u32 = 0x3a_1f_1c;
    pub const DROP_TEXT: u32 = 0xf5_a5_9b;
    pub const PUSHED_CHIP: u32 = 0x2c_24_20;
    pub const WARNING_BORDER: u32 = 0x4d_3f_22;
    pub const WARNING_TEXT: u32 = 0xe9_d3_a6;

    /// The agent prompt panel: a comment in it, and the prompt text.
    pub const PROMPT_COMMENT: u32 = 0x18_18_1b;
    pub const PROMPT: u32 = 0x0f_0f_11;

    /// A review thread under the lines it comments on.
    pub const THREAD: u32 = 0x1a1916;
    pub const THREAD_BORDER: u32 = 0x33_30_2a;
    pub const THREAD_DIVIDER: u32 = 0x2c_2a_25;
    /// Text of the amber chips: comment counts, a thread's number.
    pub const CHIP_TEXT: u32 = 0xf0_c4_6e;
    /// An open pull request's number and state chip text.
    pub const PULL_REQUEST_OPEN_TEXT: u32 = 0x7f_d8_c6;

    pub const DIFF_ADDED: u32 = 0x6c_c5_8a;
    pub const DIFF_REMOVED: u32 = 0xf0_8a_7e;

    /// Text selection: the accent at 30 % opacity.
    pub const SELECTION: u32 = 0xe8_b0_4b_4d;

    /// Initials avatar behind an author name.
    pub const AVATAR: u32 = 0x2c2c32;
    pub const AVATAR_TEXT: u32 = TEXT_SECONDARY;
    /// Author names in the history list.
    pub const AUTHOR_TEXT: u32 = 0xb8_b5_ac;
    /// Ref labels: remote-tracking branches are outlined, tags dashed.
    pub const REMOTE_LABEL_BORDER: u32 = 0x3a_3a_40;
    pub const REMOTE_LABEL_TEXT: u32 = 0xa8_a6_9e;
    pub const TAG_LABEL_BORDER: u32 = 0x5a_57_4f;
    /// Diffs: each file is a card with a header, hunk headers and lines.
    pub const DIFF_CARD: u32 = 0x141416;
    pub const DIFF_CARD_BORDER: u32 = 0x29_29_2e;
    pub const DIFF_FILE_HEADER: u32 = 0x19_19_1c;
    pub const DIFF_HUNK: u32 = 0x19_1c_22;
    pub const DIFF_HUNK_TEXT: u32 = 0x93_a9_c6;
    pub const DIFF_TEXT: u32 = 0xd8_d5_cc;
    pub const DIFF_LINE_NUMBER: u32 = 0x85_83_7b;
    /// The bar that shows unchanged lines left out of a diff.
    pub const DIFF_GAP: u32 = 0x18_18_1b;
    /// The "new file" label on a diff card.
    pub const DIFF_ADDED_TEXT: u32 = 0x8f_d6_a7;
    /// The selected file in a review's file list.
    pub const FILE_ACTIVE: u32 = BUTTON;

    /// The uncommitted-changes row of the history.
    pub const WORKING_TREE_TEXT: u32 = REMOTE_LABEL_TEXT;
    /// Text on the small HEAD badge, which takes the branch colour.
    pub const HEAD_BADGE_TEXT: u32 = 0x0e_1a_28;
}

/// Colours of the commit graph's lines, indexed by the layout's colour
/// identities modulo the length.
pub const GRAPH_COLORS: [u32; 4] = [
    color::ACCENT,
    color::BRANCH_BLUE,
    color::BRANCH_VIOLET,
    color::BRANCH_TEAL,
];

/// `hex` (`0xRRGGBB`) at `alpha` opacity, for tinted backgrounds.
pub fn with_alpha(hex: u32, alpha: f32) -> Rgba {
    let mut color = rgb(hex);
    color.a = alpha;
    color
}

/// The colour of a branch: amber for the default branch, otherwise one of the
/// branch colours, picked from the name so it stays the same between runs.
pub fn branch_color(name: &str) -> u32 {
    const COLORS: [u32; 3] = [color::BRANCH_BLUE, color::BRANCH_TEAL, color::BRANCH_VIOLET];
    if matches!(name, "main" | "master") {
        return color::ACCENT;
    }
    // FNV-1a: stable across runs and Rust releases, unlike `DefaultHasher`.
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    COLORS[hash as usize % COLORS.len()]
}

/// The UI typeface. Used only when installed; otherwise the system font.
pub const UI_FONT: &str = "Geist";
/// The typeface for code, SHAs and keyboard hints. Used only when installed.
pub const MONO_FONT: &str = "JetBrains Mono";

/// Base text size.
pub const TEXT_SIZE: Pixels = px(13.);
/// Monospace text size; slightly smaller so it sits level with the UI font.
pub const MONO_TEXT_SIZE: Pixels = px(12.);
/// Small labels: section headings, status bar, counts.
pub const SMALL_TEXT_SIZE: Pixels = px(11.5);

pub const RADIUS: Pixels = px(6.);

pub const TOOLBAR_HEIGHT: Pixels = px(48.);
pub const STATUS_BAR_HEIGHT: Pixels = px(28.);
pub const NAV_ROW_HEIGHT: Pixels = px(28.);

/// Installs the GitEZ theme. Call once, after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let is_installed = |family: &str| installed.iter().any(|name| name == family);
    let ui_font = is_installed(UI_FONT).then_some(UI_FONT);
    let mono_font = is_installed(MONO_FONT).then_some(MONO_FONT);

    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        apply_colors(&mut theme.colors);
        theme.font_size = TEXT_SIZE;
        theme.mono_font_size = MONO_TEXT_SIZE;
        if let Some(family) = ui_font {
            theme.font_family = family.into();
        }
        if let Some(family) = mono_font {
            theme.mono_font_family = family.into();
        }
        theme.radius = RADIUS;
        theme.radius_lg = px(8.);
        theme.shadow = false;
    });
}

/// Maps the GitEZ tokens onto the gpui-kit colour slots its widgets read.
fn apply_colors(colors: &mut ThemeColor) {
    use color::*;

    let c = |hex: u32| -> Hsla { rgb(hex).into() };

    colors.background = c(BACKGROUND);
    colors.foreground = c(TEXT);
    colors.border = c(BORDER);
    colors.muted = c(BUTTON);
    colors.muted_foreground = c(TEXT_MUTED);
    colors.selection = rgba(SELECTION).into();
    colors.caret = c(ACCENT);
    colors.ring = c(ACCENT);
    colors.drag_border = c(ACCENT);
    colors.overlay = rgba(0x00_00_00_80).into();
    colors.window_border = c(BORDER);

    // Hover background for menu items, list items and ghost buttons.
    colors.accent = c(BUTTON_HOVER);
    colors.accent_foreground = c(TEXT);

    colors.primary = c(ACCENT);
    colors.primary_hover = c(ACCENT_HOVER);
    colors.primary_active = c(ACCENT_HOVER);
    colors.primary_foreground = c(ON_ACCENT);
    colors.button_primary = c(ACCENT);
    colors.button_primary_hover = c(ACCENT_HOVER);
    colors.button_primary_active = c(ACCENT_HOVER);
    colors.button_primary_foreground = c(ON_ACCENT);

    colors.secondary = c(BUTTON);
    colors.secondary_hover = c(BUTTON_HOVER);
    colors.secondary_active = c(BUTTON_HOVER);
    colors.secondary_foreground = c(TEXT);
    colors.button = c(BUTTON);
    colors.button_hover = c(BUTTON_HOVER);
    colors.button_active = c(BUTTON_HOVER);
    colors.button_foreground = c(TEXT);
    colors.button_secondary = c(BUTTON);
    colors.button_secondary_hover = c(BUTTON_HOVER);
    colors.button_secondary_active = c(BUTTON_HOVER);
    colors.button_secondary_foreground = c(TEXT);
    // Default buttons draw their border with the input colour.
    colors.input = c(BUTTON_BORDER);

    colors.popover = c(PANEL);
    colors.popover_foreground = c(TEXT);

    colors.list = c(BACKGROUND);
    colors.list_even = c(BACKGROUND);
    colors.list_head = c(PANEL);
    colors.list_hover = c(ROW_HOVER);
    colors.list_active = c(SELECTED);
    colors.list_active_border = c(ACCENT);
    colors.table = c(BACKGROUND);
    colors.table_even = c(BACKGROUND);
    colors.table_head = c(BACKGROUND);
    colors.table_head_foreground = c(TEXT_FAINT);
    colors.table_hover = c(ROW_HOVER);
    colors.table_active = c(SELECTED);
    colors.table_active_border = c(ACCENT);
    colors.table_row_border = c(ROW_BORDER);

    colors.sidebar = c(SIDEBAR);
    colors.sidebar_foreground = c(TEXT_SECONDARY);
    colors.sidebar_border = c(BORDER);
    colors.sidebar_accent = c(NAV_HOVER);
    colors.sidebar_accent_foreground = c(TEXT);
    colors.sidebar_primary = c(ACCENT);
    colors.sidebar_primary_foreground = c(ON_ACCENT);

    colors.title_bar = c(PANEL);
    colors.title_bar_border = c(BORDER);
    colors.status_bar = c(PANEL);
    colors.status_bar_border = c(BORDER);
    colors.tab_bar = c(PANEL);
    colors.tab = c(PANEL);
    colors.tab_active = c(BACKGROUND);
    colors.tab_foreground = c(TEXT_MUTED);
    colors.tab_active_foreground = c(TEXT);

    colors.link = c(ACCENT);
    colors.link_hover = c(ACCENT_HOVER);
    colors.link_active = c(ACCENT_HOVER);

    colors.success = c(DIFF_ADDED);
    colors.danger = c(DIFF_REMOVED);
    colors.warning = c(ACCENT);
    colors.info = c(BRANCH_BLUE);
    colors.green = c(DIFF_ADDED);
    colors.red = c(DIFF_REMOVED);
    colors.yellow = c(ACCENT);
    colors.blue = c(BRANCH_BLUE);
    colors.magenta = c(BRANCH_VIOLET);
    colors.cyan = c(BRANCH_TEAL);
}
