//! The icons GitEZ draws.
//!
//! gpui-kit embeds only the icons its own components use. Every other icon
//! named with `IconName` must be listed in [`ExtraIcons`], or it renders as
//! an empty space.

use std::borrow::Cow;

use gpui_kit::assets::icon_assets;
use gpui_kit::*;

icon_assets!(
    ExtraIcons,
    [
        Archive,
        ArrowRight,
        Bot,
        Cloud,
        Copy,
        GitBranch,
        GitGraph,
        GitPullRequestArrow,
        ListTree,
        Square,
        Terminal
    ]
);

/// gpui-kit's default icons plus [`ExtraIcons`].
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::Assets;
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;

    /// Every icon named in the source must load, or it would render blank.
    #[test]
    fn every_icon_used_is_embedded() {
        let source_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut names = BTreeSet::new();
        let mut dirs = vec![std::path::PathBuf::from(source_dir)];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).expect("source directory is readable") {
                let path = entry.expect("directory entry is readable").path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("source file is readable");
                for rest in text.split("IconName::").skip(1) {
                    let name: String = rest
                        .chars()
                        .take_while(char::is_ascii_alphanumeric)
                        .collect();
                    names.insert(name);
                }
            }
        }
        let missing: Vec<_> = IconName::ALL
            .iter()
            .filter(|icon| names.contains(&format!("{icon:?}")))
            .filter(|icon| !matches!(Assets.load(&icon.path()), Ok(Some(_))))
            .map(|icon| format!("{icon:?}"))
            .collect();
        assert!(missing.is_empty(), "add to ExtraIcons: {missing:?}");
    }
}
