//! Finding the icon of an open repository: its favicon, app icon or logo.

use std::path::Path;

use gitez_git::Repository;
use gpui_kit::{Image, ImageFormat};

/// Deepest a tracked file can be and still be taken for the project's icon.
const MAX_DEPTH: usize = 5;

/// Largest file read as an icon.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Width, in pixels, an icon needs to look sharp in the toolbar: its 22 points
/// on a 2x display.
const SHARP_WIDTH: u32 = 44;

/// Folders whose icons belong to something other than the project.
const OTHER_PROJECTS: [&str; 14] = [
    "__tests__",
    "demo",
    "demos",
    "example",
    "examples",
    "fixtures",
    "node_modules",
    "sample",
    "samples",
    "test",
    "testdata",
    "tests",
    "third_party",
    "vendor",
];

/// File extensions an icon can have, most preferred first.
const EXTENSIONS: [&str; 7] = ["svg", "png", "webp", "icns", "ico", "jpg", "jpeg"];

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The best icon among the files tracked in `repository`, which is called
/// `name`, if one can be read. Blocks; run it on a background thread.
pub fn find(repository: &Repository, name: &str) -> Option<Image> {
    let work_dir = repository.work_dir()?;
    let name = name.to_lowercase();
    let mut candidates = Vec::new();
    repository
        .visit_tracked_paths(|path| {
            if let Some(rank) = rank(path, &name) {
                candidates.push((rank, path.to_owned()));
            }
        })
        .ok()?;
    candidates.sort();
    candidates
        .into_iter()
        .find_map(|(_, path)| load(&work_dir.join(path)))
}

/// How good an icon the tracked file `path` would be for the repository
/// `name`, lower being better, or `None` if it is not one. `name` is lower
/// case.
fn rank(path: &str, name: &str) -> Option<(u8, usize, usize)> {
    let (folder, file) = path.rsplit_once('/').unwrap_or(("", path));
    let file = file.to_lowercase();
    let (stem, extension) = file.rsplit_once('.')?;
    let preference = EXTENSIONS.iter().position(|known| *known == extension)?;
    let kind = match stem {
        "favicon" => 0,
        "appicon" | "app-icon" | "app_icon" | "apple-touch-icon" => 1,
        _ if extension == "icns" => 2,
        "icon" => 3,
        "logo" => 4,
        _ if stem == name => 5,
        _ => return None,
    };
    let folders: Vec<&str> = folder.split('/').filter(|part| !part.is_empty()).collect();
    let elsewhere = folders
        .iter()
        .any(|part| OTHER_PROJECTS.contains(&part.to_lowercase().as_str()));
    (!elsewhere && folders.len() < MAX_DEPTH).then_some((kind, preference, folders.len()))
}

/// The icon in the file at `path`, if it is a regular file of a sensible
/// size in a format that can be drawn. Whether its pixels decode is found out
/// when it is drawn.
fn load(path: &Path) -> Option<Image> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let extension = path.extension()?.to_str()?.to_lowercase();
    if extension == "icns" {
        let png = icns_png(&bytes)?.to_vec();
        return Some(Image::from_bytes(ImageFormat::Png, png));
    }
    // A PNG renamed to `favicon.ico` is common, and browsers accept it.
    let format = if bytes.starts_with(PNG_SIGNATURE) {
        ImageFormat::Png
    } else {
        match extension.as_str() {
            "svg" => ImageFormat::Svg,
            "webp" => ImageFormat::Webp,
            "ico" => ImageFormat::Ico,
            "jpg" | "jpeg" => ImageFormat::Jpeg,
            _ => return None,
        }
    };
    Some(Image::from_bytes(format, bytes))
}

/// The PNG inside the Apple icon file `icns` that suits the toolbar best: the
/// smallest that is still sharp, or else the largest. Icons stored in the
/// other formats of that file are not considered.
fn icns_png(icns: &[u8]) -> Option<&[u8]> {
    let mut rest = icns.strip_prefix(b"icns")?.get(4..)?;
    let mut images = Vec::new();
    // Each entry is a four-byte type, then its big-endian length including
    // these eight bytes, then its data.
    while let Some((header, after)) = rest.split_first_chunk::<8>() {
        let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let Some(data) = length.checked_sub(8).and_then(|size| after.get(..size)) else {
            break;
        };
        images.extend(png_width(data).map(|width| (width, data)));
        rest = &after[data.len()..];
    }
    images
        .into_iter()
        .min_by_key(|&(width, _)| {
            if width >= SHARP_WIDTH {
                (false, width)
            } else {
                (true, u32::MAX - width)
            }
        })
        .map(|(_, data)| data)
}

/// The width of the PNG image `data`, if it is one.
fn png_width(data: &[u8]) -> Option<u32> {
    if !data.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let width: [u8; 4] = data.get(16..20)?.try_into().ok()?;
    Some(u32::from_be_bytes(width))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG header that says the image is `width` pixels wide.
    fn png(width: u32) -> Vec<u8> {
        let mut data = PNG_SIGNATURE.to_vec();
        data.extend([0, 0, 0, 13]);
        data.extend(b"IHDR");
        data.extend(width.to_be_bytes());
        data.extend(width.to_be_bytes());
        data
    }

    fn icns(entries: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (kind, data) in entries {
            body.extend(*kind);
            body.extend((data.len() as u32 + 8).to_be_bytes());
            body.extend(data);
        }
        let mut file = b"icns".to_vec();
        file.extend((body.len() as u32 + 8).to_be_bytes());
        file.extend(body);
        file
    }

    fn best<'a>(paths: &[&'a str], name: &str) -> Option<&'a str> {
        paths
            .iter()
            .filter_map(|path| Some((rank(path, name)?, *path)))
            .min()
            .map(|(_, path)| path)
    }

    #[test]
    fn a_favicon_beats_an_app_icon_and_a_logo() {
        assert_eq!(
            best(
                &["docs/logo.svg", "macos/App.icns", "public/favicon.ico"],
                "app"
            ),
            Some("public/favicon.ico")
        );
    }

    #[test]
    fn an_app_icon_is_found_under_any_name() {
        assert_eq!(
            best(&["crates/gitez/assets/GitEZ.icns"], "gitez"),
            Some("crates/gitez/assets/GitEZ.icns")
        );
    }

    #[test]
    fn a_vector_icon_beats_a_bitmap_and_a_shallow_one_a_deep_one() {
        assert_eq!(best(&["icon.png", "icon.svg"], "app"), Some("icon.svg"));
        assert_eq!(
            best(&["apps/web/logo.png", "assets/logo.png"], "app"),
            Some("assets/logo.png")
        );
    }

    #[test]
    fn an_image_named_after_the_repository_counts() {
        assert_eq!(
            best(&["assets/GitEZ.png"], "gitez"),
            Some("assets/GitEZ.png")
        );
        assert_eq!(best(&["assets/banner.png"], "gitez"), None);
    }

    #[test]
    fn icons_of_other_projects_do_not_count() {
        assert_eq!(best(&["node_modules/pkg/logo.png"], "app"), None);
        assert_eq!(best(&["examples/web/favicon.ico"], "app"), None);
        assert_eq!(best(&["a/b/c/d/e/favicon.ico"], "app"), None);
    }

    #[test]
    fn the_smallest_sharp_png_is_taken_from_an_icon_file() {
        let file = icns(&[
            (b"TOC ", vec![0; 8]),
            (b"ic11", png(32)),
            (b"ic13", png(256)),
            (b"ic12", png(64)),
        ]);
        assert_eq!(icns_png(&file).and_then(png_width), Some(64));
    }

    #[test]
    fn the_largest_png_is_taken_when_none_is_sharp() {
        let file = icns(&[(b"icp4", png(16)), (b"ic11", png(32))]);
        assert_eq!(icns_png(&file).and_then(png_width), Some(32));
    }

    #[test]
    fn a_broken_icon_file_has_no_image() {
        assert_eq!(icns_png(b"not an icon file"), None);
        let mut truncated = icns(&[(b"ic12", png(64))]);
        truncated.truncate(20);
        assert_eq!(icns_png(&truncated), None);
    }

    #[test]
    fn a_png_named_ico_is_read_as_a_png() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let path = dir.path().join("favicon.ico");
        std::fs::write(&path, png(32)).expect("write the icon");
        let image = load(&path).expect("the icon loads");
        assert_eq!(image.format, ImageFormat::Png);
    }

    #[test]
    fn a_file_too_big_is_not_read() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let path = dir.path().join("logo.png");
        let file = std::fs::File::create(&path).expect("create the icon");
        file.set_len(MAX_FILE_BYTES + 1).expect("size the icon");
        assert!(load(&path).is_none());
    }

    #[test]
    fn finds_a_tracked_icon_and_skips_a_missing_one() {
        let dir = tempfile::TempDir::new().expect("create temp dir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        std::fs::write(root.join("favicon.png"), png(32)).expect("write the favicon");
        std::fs::write(root.join("logo.png"), png(48)).expect("write the logo");
        let repository = Repository::open(root).expect("open the repository");

        // Neither is tracked yet.
        assert!(find(&repository, "app").is_none());

        git(&["add", "favicon.png", "logo.png"]);
        let repository = Repository::open(root).expect("open the repository");
        let favicon = Image::from_bytes(ImageFormat::Png, png(32));
        assert_eq!(
            find(&repository, "app").map(|icon| icon.id()),
            Some(favicon.id())
        );

        // Deleted from the working tree: the next best one is used.
        std::fs::remove_file(root.join("favicon.png")).expect("delete the favicon");
        let logo = Image::from_bytes(ImageFormat::Png, png(48));
        assert_eq!(
            find(&repository, "app").map(|icon| icon.id()),
            Some(logo.id())
        );
    }
}
