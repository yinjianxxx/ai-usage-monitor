//! Shared provider-brand tiles used by compact surfaces and no-data tray icons.

use std::sync::Mutex;
use windows::Win32::Foundation::TRUE;
use windows::Win32::UI::WindowsAndMessaging::{CreateIconFromResourceEx, HICON, LR_DEFAULTCOLOR};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ProviderBrand {
    Claude,
    Codex,
    Antigravity,
    Grok,
}

pub(crate) const BUCKET_DPIS: [u32; 10] = [96, 120, 144, 168, 192, 216, 240, 288, 336, 384];
const CHIP16_SIZES: [i32; 10] = [16, 20, 24, 28, 32, 36, 40, 48, 56, 64];

const CLAUDE_DARK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c16-384.png"),
];
const CLAUDE_LIGHT: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c16-384.png"),
];
const OPENAI: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/openai-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c16-384.png"),
];
const GROK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/grok-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c16-384.png"),
];
const ANTIGRAVITY_DARK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c16-384.png"),
];
const ANTIGRAVITY_LIGHT: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c16-384.png"),
];

#[derive(Clone, Copy)]
pub(crate) struct Chip16Asset {
    pub(crate) bucket: usize,
    pub(crate) size: i32,
    pub(crate) bytes: &'static [u8],
}

pub(crate) fn nearest_bucket(dpi: u32) -> usize {
    let dpi = if dpi == 0 { 96 } else { dpi };
    let mut best = 0;
    let mut best_distance = dpi.abs_diff(BUCKET_DPIS[0]);
    for (index, bucket_dpi) in BUCKET_DPIS.iter().enumerate().skip(1) {
        let distance = dpi.abs_diff(*bucket_dpi);
        if distance < best_distance {
            best = index;
            best_distance = distance;
        }
    }
    best
}

pub(crate) fn chip16_asset(brand: ProviderBrand, dpi: u32, is_dark: bool) -> Chip16Asset {
    let bucket = nearest_bucket(dpi);
    let bytes = match (brand, is_dark) {
        (ProviderBrand::Claude, true) => CLAUDE_DARK[bucket],
        (ProviderBrand::Claude, false) => CLAUDE_LIGHT[bucket],
        (ProviderBrand::Codex, _) => OPENAI[bucket],
        (ProviderBrand::Antigravity, true) => ANTIGRAVITY_DARK[bucket],
        (ProviderBrand::Antigravity, false) => ANTIGRAVITY_LIGHT[bucket],
        (ProviderBrand::Grok, _) => GROK[bucket],
    };
    let size = CHIP16_SIZES[bucket];
    debug_assert_eq!(size, scale_px_for_dpi(16, BUCKET_DPIS[bucket]));
    Chip16Asset {
        bucket,
        size,
        bytes,
    }
}

type CacheEntry = ((ProviderBrand, usize, bool), isize);
static CACHE: Mutex<Vec<CacheEntry>> = Mutex::new(Vec::new());

/// Return a process-lifetime cached HICON for repeated GDI painting.
pub(crate) fn cached_chip16_icon(
    brand: ProviderBrand,
    dpi: u32,
    is_dark: bool,
    high_contrast: bool,
) -> Option<(HICON, i32)> {
    if high_contrast {
        return None;
    }
    let asset = chip16_asset(brand, dpi, is_dark);
    let key = (brand, asset.bucket, is_dark);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, handle)) = cache.iter().find(|(cached_key, _)| *cached_key == key) {
        return Some((HICON(*handle as *mut _), asset.size));
    }
    let hicon = create_icon(asset)?;
    cache.push((key, hicon.0 as isize));
    Some((hicon, asset.size))
}

/// Create an owned HICON. The caller must destroy it after handing it to Shell.
pub(crate) fn create_chip16_icon(
    brand: ProviderBrand,
    dpi: u32,
    is_dark: bool,
) -> Option<(HICON, i32)> {
    let asset = chip16_asset(brand, dpi, is_dark);
    create_icon(asset).map(|icon| (icon, asset.size))
}

fn create_icon(asset: Chip16Asset) -> Option<HICON> {
    unsafe {
        CreateIconFromResourceEx(
            asset.bytes,
            TRUE,
            0x0003_0000,
            asset.size,
            asset.size,
            LR_DEFAULTCOLOR,
        )
        .ok()
    }
}

fn scale_px_for_dpi(px: i32, dpi: u32) -> i32 {
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip16_bucket_selection_clamps_and_breaks_ties_downward() {
        for (index, dpi) in BUCKET_DPIS.iter().enumerate() {
            assert_eq!(nearest_bucket(*dpi), index);
        }
        assert_eq!(nearest_bucket(0), 0);
        assert_eq!(nearest_bucket(72), 0);
        for (index, pair) in BUCKET_DPIS.windows(2).enumerate() {
            let midpoint = (pair[0] + pair[1]) / 2;
            assert_eq!(nearest_bucket(midpoint), index);
            assert_eq!(nearest_bucket(midpoint + 1), index + 1);
        }
        assert_eq!(nearest_bucket(480), BUCKET_DPIS.len() - 1);
    }
}
