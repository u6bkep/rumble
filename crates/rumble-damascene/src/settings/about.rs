//! About tab: build / version info from `build.rs` git metadata.

use super::*;

/// Git-derived build metadata shown on the About tab. Real builds read the
/// compile-time values `build.rs` bakes in; the golden harness pins a fixed
/// fixture via [`set_git_build_info_override`] so the commit hash and
/// `git describe` don't drift on every commit. Only the string contents
/// differ between the two — the About layout (ellipsis, fill width, the
/// dirty/clean color tokens) renders identically, so lint coverage of the
/// scene is unchanged.
#[derive(Clone)]
pub struct GitBuildInfo {
    pub describe: String,
    pub short_hash: String,
    pub full_hash: String,
    /// Empty when HEAD isn't tagged.
    pub tag: String,
    pub dirty: bool,
}

impl GitBuildInfo {
    /// The genuine compile-time values populated by `build.rs`.
    fn from_env() -> Self {
        GitBuildInfo {
            describe: env!("RUMBLE_DAMASCENE_GIT_DESCRIBE").to_string(),
            short_hash: env!("RUMBLE_DAMASCENE_GIT_SHORT_HASH").to_string(),
            full_hash: env!("RUMBLE_DAMASCENE_GIT_HASH").to_string(),
            tag: env!("RUMBLE_DAMASCENE_GIT_TAG").to_string(),
            dirty: env!("RUMBLE_DAMASCENE_GIT_DIRTY") == "true",
        }
    }
}

static GIT_BUILD_INFO_OVERRIDE: std::sync::OnceLock<GitBuildInfo> = std::sync::OnceLock::new();

/// Pin the About-tab git metadata to a fixed fixture. Intended for the
/// `dump_bundles` golden harness only — the real app never calls this and so
/// always renders the true `build.rs` values. First call wins; later calls
/// are ignored.
pub fn set_git_build_info_override(info: GitBuildInfo) {
    let _ = GIT_BUILD_INFO_OVERRIDE.set(info);
}

fn git_build_info() -> GitBuildInfo {
    GIT_BUILD_INFO_OVERRIDE
        .get()
        .cloned()
        .unwrap_or_else(GitBuildInfo::from_env)
}

/// Build / version info. `pkg_version` is the (deterministic) crate version;
/// the git fields come from [`git_build_info`] so the golden harness can pin
/// them. Tag is empty when HEAD isn't tagged; dirty is "true" / "false".
pub(super) fn render_about() -> El {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let info = git_build_info();
    let describe = info.describe.as_str();
    let short_hash = info.short_hash.as_str();
    let full_hash = info.full_hash.as_str();
    let tag = info.tag.as_str();
    let dirty = info.dirty;

    let mut rows: Vec<El> = Vec::new();
    rows.push(field_row("Package version", text(pkg_version).semibold()));

    if !tag.is_empty() {
        rows.push(field_row("Tag", text(tag).semibold()));
    }
    rows.push(field_row(
        "Commit",
        mono(format!("{short_hash} ({full_hash})"))
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));
    rows.push(field_row(
        "Working tree",
        if dirty {
            text("dirty — built from uncommitted changes").text_color(tokens::WARNING)
        } else {
            text("clean").text_color(tokens::SUCCESS)
        },
    ));
    rows.push(field_row(
        "git describe",
        mono(describe)
            .font_size(tokens::TEXT_XS.size)
            .ellipsis()
            .width(Size::Fill(1.0)),
    ));

    column([section_card("Rumble", rows)])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
}
