use std::time::Duration;

// Embed animation frames for each variant at compile time.
macro_rules! frames_for {
    ($dir:literal) => {
        [
            include_str!(concat!("../frames/", $dir, "/frame_1.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_2.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_3.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_4.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_5.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_6.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_7.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_8.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_9.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_10.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_11.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_12.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_13.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_14.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_15.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_16.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_17.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_18.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_19.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_20.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_21.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_22.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_23.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_24.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_25.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_26.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_27.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_28.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_29.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_30.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_31.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_32.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_33.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_34.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_35.txt")),
            include_str!(concat!("../frames/", $dir, "/frame_36.txt")),
        ]
    };
}

pub(crate) const FRAMES_DEFAULT: [&str; 36] = frames_for!("default");
pub(crate) const FRAMES_CODEX: [&str; 36] = frames_for!("codex");
pub(crate) const FRAMES_OPENAI: [&str; 36] = frames_for!("openai");
pub(crate) const FRAMES_BLOCKS: [&str; 36] = frames_for!("blocks");
pub(crate) const FRAMES_DOTS: [&str; 36] = frames_for!("dots");
pub(crate) const FRAMES_HASH: [&str; 36] = frames_for!("hash");
pub(crate) const FRAMES_HBARS: [&str; 36] = frames_for!("hbars");
pub(crate) const FRAMES_VBARS: [&str; 36] = frames_for!("vbars");
pub(crate) const FRAMES_SHAPES: [&str; 36] = frames_for!("shapes");
pub(crate) const FRAMES_SLUG: [&str; 36] = frames_for!("slug");

/// Upstream's Ore wordmark sets, retained but not shown.
///
/// ore ships its own art (below), so nothing selects these. They are kept —
/// constants and frame files both — so the fork carries no deletion diff
/// against upstream and edits to those files merge instead of conflicting.
#[expect(dead_code, reason = "kept to keep the upstream diff additive")]
pub(crate) const UPSTREAM_VARIANTS: &[&[&str]] = &[
    &FRAMES_DEFAULT,
    &FRAMES_CODEX,
    &FRAMES_OPENAI,
    &FRAMES_BLOCKS,
    &FRAMES_DOTS,
    &FRAMES_HASH,
    &FRAMES_HBARS,
    &FRAMES_VBARS,
    &FRAMES_SHAPES,
    &FRAMES_SLUG,
];

// The rotating ore crystal, rendered by `scripts/ore_art.py` and baked in by
// `scripts/generate-ore-frames.py`.
//
// Two dimensions: size and character ramp. `welcome.rs` picks the largest size
// that fits the terminal, so the logo scales instead of being a postage stamp
// on a big screen or overflowing a small one. Within a size, `.` cycles ramps —
// the same variant mechanic upstream's sets used.
//
// Regenerate with `just frames`; verify with `just frames-check`.
pub(crate) const FRAMES_SMALL_MINERAL: [&str; 36] = frames_for!("ore-small-mineral");
pub(crate) const FRAMES_SMALL_BLOCKS: [&str; 36] = frames_for!("ore-small-blocks");
pub(crate) const FRAMES_SMALL_FINE: [&str; 36] = frames_for!("ore-small-fine");
pub(crate) const FRAMES_MEDIUM_MINERAL: [&str; 36] = frames_for!("ore-medium-mineral");
pub(crate) const FRAMES_MEDIUM_BLOCKS: [&str; 36] = frames_for!("ore-medium-blocks");
pub(crate) const FRAMES_MEDIUM_FINE: [&str; 36] = frames_for!("ore-medium-fine");
pub(crate) const FRAMES_LARGE_MINERAL: [&str; 36] = frames_for!("ore-large-mineral");
pub(crate) const FRAMES_LARGE_BLOCKS: [&str; 36] = frames_for!("ore-large-blocks");
pub(crate) const FRAMES_LARGE_FINE: [&str; 36] = frames_for!("ore-large-fine");

pub(crate) const VARIANTS_SMALL: &[&[&str]] = &[
    &FRAMES_SMALL_MINERAL,
    &FRAMES_SMALL_BLOCKS,
    &FRAMES_SMALL_FINE,
];
pub(crate) const VARIANTS_MEDIUM: &[&[&str]] = &[
    &FRAMES_MEDIUM_MINERAL,
    &FRAMES_MEDIUM_BLOCKS,
    &FRAMES_MEDIUM_FINE,
];
pub(crate) const VARIANTS_LARGE: &[&[&str]] = &[
    &FRAMES_LARGE_MINERAL,
    &FRAMES_LARGE_BLOCKS,
    &FRAMES_LARGE_FINE,
];

/// Default set, used where the render area is not known.
pub(crate) const ALL_VARIANTS: &[&[&str]] = VARIANTS_MEDIUM;

/// Rendered size of each set, as (cols, rows). Must match `SIZES` in
/// `scripts/generate-ore-frames.py`; `frames-check` is what enforces that.
pub(crate) const SIZE_SMALL: (u16, u16) = (38, 16);
pub(crate) const SIZE_MEDIUM: (u16, u16) = (46, 20);
pub(crate) const SIZE_LARGE: (u16, u16) = (62, 26);

/// Largest crystal that fits, leaving `reserved_rows` for the text beneath it.
///
/// Returns `None` when even the small one would not fit, which is the caller's
/// signal to skip the animation rather than render a clipped frame.
pub(crate) fn variants_for_area(
    width: u16,
    height: u16,
    reserved_rows: u16,
) -> Option<&'static [&'static [&'static str]]> {
    let fits = |(cols, rows): (u16, u16)| width >= cols && height >= rows + reserved_rows;
    if fits(SIZE_LARGE) {
        Some(VARIANTS_LARGE)
    } else if fits(SIZE_MEDIUM) {
        Some(VARIANTS_MEDIUM)
    } else if fits(SIZE_SMALL) {
        Some(VARIANTS_SMALL)
    } else {
        None
    }
}

pub(crate) const FRAME_TICK_DEFAULT: Duration = Duration::from_millis(80);

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames are `include_str!`d, so a set whose files were regenerated at
    /// the wrong geometry would render ragged or clipped rather than fail to
    /// build. Every row is padded to exactly `cols` visible columns.
    #[test]
    fn every_frame_matches_its_declared_size() {
        let ansi = regex_lite::Regex::new("\x1b\\[[0-9;]*m").expect("valid regex");
        for (name, variants, (cols, rows)) in [
            ("small", VARIANTS_SMALL, SIZE_SMALL),
            ("medium", VARIANTS_MEDIUM, SIZE_MEDIUM),
            ("large", VARIANTS_LARGE, SIZE_LARGE),
        ] {
            for (vi, frames) in variants.iter().enumerate() {
                for (fi, frame) in frames.iter().enumerate() {
                    let lines: Vec<&str> = frame.split('\n').collect();
                    assert_eq!(
                        lines.len(),
                        rows as usize,
                        "{name} variant {vi} frame {fi}: wrong row count"
                    );
                    for (li, line) in lines.iter().enumerate() {
                        assert_eq!(
                            ansi.replace_all(line, "").chars().count(),
                            cols as usize,
                            "{name} variant {vi} frame {fi} row {li}: wrong visible width"
                        );
                    }
                }
            }
        }
    }

    /// A frame whose row count differs from its neighbours is the bobbing bug:
    /// the crystal jumps vertically as the silhouette changes through the spin.
    #[test]
    fn row_count_is_constant_within_a_set() {
        for (name, variants) in [
            ("small", VARIANTS_SMALL),
            ("medium", VARIANTS_MEDIUM),
            ("large", VARIANTS_LARGE),
        ] {
            for (vi, frames) in variants.iter().enumerate() {
                let counts: std::collections::BTreeSet<usize> =
                    frames.iter().map(|f| f.split('\n').count()).collect();
                assert_eq!(
                    counts.len(),
                    1,
                    "{name} variant {vi}: row count varies across frames: {counts:?}"
                );
            }
        }
    }
}
