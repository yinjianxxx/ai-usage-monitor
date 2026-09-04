use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_INFO,
    NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_NONE, NIIF_NOSOUND, NIIF_USER,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION,
    NIN_BALLOONUSERCLICK, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
    NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::compact_view;
use crate::diagnose;
use crate::native_interop::{self, Color, WM_APP_TRAY};
use crate::provider_tile::{self, ProviderBrand};
use crate::theme;

const CLAUDE_TRAY_ICON_ID: u32 = 1;
const CODEX_TRAY_ICON_ID: u32 = 2;
const ANTIGRAVITY_TRAY_ICON_ID: u32 = 3;
const APP_TRAY_ICON_ID: u32 = 4;
const GROK_TRAY_ICON_ID: u32 = 5;
const APP_ICON_RESOURCE_ID: usize = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdentityMode {
    Guid,
    LegacyUId,
}

static CLAUDE_LEGACY_UID: AtomicBool = AtomicBool::new(false);
static CODEX_LEGACY_UID: AtomicBool = AtomicBool::new(false);
static ANTIGRAVITY_LEGACY_UID: AtomicBool = AtomicBool::new(false);
static GROK_LEGACY_UID: AtomicBool = AtomicBool::new(false);
static APP_LEGACY_UID: AtomicBool = AtomicBool::new(false);

const ICON_SIZE: i32 = 64;
const TRAY_BRAND_TILE_DPI: u32 = 384;
const BAR_LEFT: i32 = 0;
const BAR_RIGHT: i32 = ICON_SIZE;
const BAR_5H_TOP: i32 = 42;
const BAR_7D_TOP: i32 = 55;
const BAR_HEIGHT: i32 = 9;
const SINGLE_BAR_TOP: i32 = 48;
const SINGLE_BAR_HEIGHT: i32 = 13;
const NUMBER_TOP: i32 = 0;
const NUMBER_BOTTOM: i32 = 38;

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 70;

/// Actions the tray message handler can request from the main window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    None,
    ShowDetails {
        kind: Option<TrayIconKind>,
        keyboard: bool,
    },
    ShowContextMenu {
        kind: Option<TrayIconKind>,
        anchor_to_icon: bool,
    },
    /// The user clicked the balloon itself. What that offers, if anything, is
    /// `take_balloon_click`; the icon it arrived on says nothing, because a
    /// balloon is delivered to whichever icon the tray happened to have.
    BalloonClicked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayIconKind {
    Claude,
    Codex,
    Antigravity,
    Grok,
}

pub struct TrayIconData {
    pub kind: TrayIconKind,
    /// Canonical headline first, then the most urgent remaining window.
    pub percents: Vec<f64>,
    pub tooltip: String,
}

impl TrayIconKind {
    /// Every provider, in the canonical order used to index per-provider
    /// storage. Adding a variant here is what makes it visible to the
    /// settings sweep, the poll pass, and `AppUsageData`.
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Antigravity, Self::Grok];
    pub const COUNT: usize = Self::ALL.len();

    /// Position in [`Self::ALL`]. Only ever used as an array index; it is not
    /// persisted, so the order can change without a settings migration.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
            Self::Antigravity => 2,
            Self::Grok => 3,
        }
    }

    /// Fixed English name for diagnostics. Never localized: log lines are
    /// read by whoever is debugging, not by the user.
    pub(crate) const fn diagnostic_label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Antigravity => "Antigravity",
            Self::Grok => "Grok",
        }
    }

    pub(crate) fn brand(self) -> ProviderBrand {
        match self {
            Self::Claude => ProviderBrand::Claude,
            Self::Codex => ProviderBrand::Codex,
            Self::Antigravity => ProviderBrand::Antigravity,
            Self::Grok => ProviderBrand::Grok,
        }
    }

    fn id(self) -> u32 {
        match self {
            Self::Claude => CLAUDE_TRAY_ICON_ID,
            Self::Codex => CODEX_TRAY_ICON_ID,
            Self::Antigravity => ANTIGRAVITY_TRAY_ICON_ID,
            Self::Grok => GROK_TRAY_ICON_ID,
        }
    }

    fn from_id(id: u32) -> Option<Self> {
        match id {
            CLAUDE_TRAY_ICON_ID => Some(Self::Claude),
            CODEX_TRAY_ICON_ID => Some(Self::Codex),
            ANTIGRAVITY_TRAY_ICON_ID => Some(Self::Antigravity),
            GROK_TRAY_ICON_ID => Some(Self::Grok),
            _ => None,
        }
    }

    fn guid(self) -> GUID {
        match self {
            Self::Claude => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f01),
            Self::Codex => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f02),
            Self::Antigravity => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f03),
            Self::Grok => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f04),
        }
    }

    fn legacy_uid_flag(self) -> &'static AtomicBool {
        match self {
            Self::Claude => &CLAUDE_LEGACY_UID,
            Self::Codex => &CODEX_LEGACY_UID,
            Self::Antigravity => &ANTIGRAVITY_LEGACY_UID,
            Self::Grok => &GROK_LEGACY_UID,
        }
    }

    fn identity_mode(self) -> IdentityMode {
        if self.legacy_uid_flag().load(Ordering::Relaxed) {
            IdentityMode::LegacyUId
        } else {
            IdentityMode::Guid
        }
    }

    fn use_legacy_uid(self) {
        self.legacy_uid_flag().store(true, Ordering::Relaxed);
    }
}

fn app_icon_guid() -> GUID {
    GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f00)
}

fn app_identity_mode() -> IdentityMode {
    if APP_LEGACY_UID.load(Ordering::Relaxed) {
        IdentityMode::LegacyUId
    } else {
        IdentityMode::Guid
    }
}

fn use_legacy_app_uid() {
    APP_LEGACY_UID.store(true, Ordering::Relaxed);
}

/// Query the screen rectangle of one of this process's notification icons.
/// The public Shell API identifies our icons by their shared owner window and
/// distinct uID values, matching how `ensure` registers them.
pub fn rect(hwnd: HWND, kind: TrayIconKind) -> Option<RECT> {
    let mode = kind.identity_mode();
    rect_for_identity(hwnd, kind.id(), kind.guid(), mode)
}

pub fn app_rect(hwnd: HWND) -> Option<RECT> {
    rect_for_identity(hwnd, APP_TRAY_ICON_ID, app_icon_guid(), app_identity_mode())
}

fn rect_for_identity(hwnd: HWND, id: u32, guid: GUID, mode: IdentityMode) -> Option<RECT> {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: id,
        guidItem: if mode == IdentityMode::Guid {
            guid
        } else {
            GUID::zeroed()
        },
    };
    unsafe { Shell_NotifyIconGetRect(&identifier).ok() }
}

/// Read and validate the left-to-right (or top-to-bottom for a vertical
/// taskbar) order of this app's visible notification icons. All requested
/// icons must resolve to distinct rectangles on the selected taskbar; hidden
/// overflow icons and partial Shell results deliberately produce no order.
pub fn visible_order(
    hwnd: HWND,
    kinds: &[TrayIconKind],
    taskbar_rect: &RECT,
) -> Option<Vec<TrayIconKind>> {
    if kinds.len() <= 1 {
        return Some(kinds.to_vec());
    }

    let positions = kinds
        .iter()
        .copied()
        .map(|kind| rect(hwnd, kind).map(|rect| (kind, rect)))
        .collect::<Option<Vec<_>>>()?;
    order_from_rects(&positions, taskbar_rect)
}

fn order_from_rects(
    positions: &[(TrayIconKind, RECT)],
    taskbar_rect: &RECT,
) -> Option<Vec<TrayIconKind>> {
    let taskbar_width = taskbar_rect.right - taskbar_rect.left;
    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    if taskbar_width <= 0 || taskbar_height <= 0 {
        return None;
    }
    let horizontal = taskbar_width >= taskbar_height;

    let mut located = Vec::with_capacity(positions.len());
    let mut cross_min = i32::MAX;
    let mut cross_max = i32::MIN;
    let mut max_cross_extent = 0;
    for (kind, rect) in positions {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return None;
        }
        let center_x = rect.left + width / 2;
        let center_y = rect.top + height / 2;
        if center_x < taskbar_rect.left
            || center_x >= taskbar_rect.right
            || center_y < taskbar_rect.top
            || center_y >= taskbar_rect.bottom
        {
            return None;
        }

        let axis = if horizontal { center_x } else { center_y };
        let cross = if horizontal { center_y } else { center_x };
        let cross_extent = if horizontal { height } else { width };
        cross_min = cross_min.min(cross);
        cross_max = cross_max.max(cross);
        max_cross_extent = max_cross_extent.max(cross_extent);
        located.push((*kind, axis));
    }

    // A thick legacy taskbar can arrange icons in multiple rows. There is no
    // unambiguous single left/right order in that layout, so retain the last
    // valid widget order instead of guessing.
    if cross_max - cross_min > max_cross_extent / 2 {
        return None;
    }

    located.sort_by_key(|(_, axis)| *axis);
    if located.windows(2).any(|pair| pair[0].1 == pair[1].1) {
        return None;
    }
    Some(located.into_iter().map(|(kind, _)| kind).collect())
}

/// Provider base colour for the icon number/bars: Claude orange, Codex
/// monochrome against the taskbar theme, Antigravity Google blue. Fixed
/// brand colours (no usage gradient - its mid-range read as yellow), same
/// language as the widget accents and the detail popup.
fn provider_color(kind: TrayIconKind, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        return theme::system_color(COLOR_HIGHLIGHT);
    }
    match kind {
        TrayIconKind::Claude => Color::from_hex("#D97757"),
        TrayIconKind::Codex => {
            if is_dark {
                Color::from_hex("#F5F5F5")
            } else {
                Color::from_hex("#111111")
            }
        }
        TrayIconKind::Antigravity => Color::from_hex("#4285F4"),
        // Deliberately not xAI's black-and-white: that is exactly Codex's
        // tray colour, and two providers whose numbers render identically
        // defeat the point of separate icons. The brand's own look is kept
        // on the no-data tile.
        TrayIconKind::Grok => Color::from_hex("#7C6BF5"),
    }
}

/// Number/bar colour: the provider colour, switching to warning red near the
/// limit so a nearly-exhausted window is visible at a glance.
fn number_color(kind: TrayIconKind, percent: f64, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else if compact_view::display_percent_warns(percent) {
        Color::from_hex("#E5484D")
    } else {
        provider_color(kind, is_dark, false)
    }
}

/// High Contrast and decode-failure fallback while no usage data is available:
/// provider-company initials avoid the Claude/Codex "C" collision.
fn placeholder_letter(kind: TrayIconKind) -> &'static str {
    match kind {
        TrayIconKind::Claude => "A",
        TrayIconKind::Codex => "O",
        TrayIconKind::Antigravity => "G",
        // xAI, because Antigravity already holds "G".
        TrayIconKind::Grok => "X",
    }
}

fn bar_track_color(is_dark: bool, high_contrast: bool) -> Color {
    // Keep the track well away from the mid-tone provider fills (Claude's
    // darker oranges especially) so the filled portion reads at 16px.
    if high_contrast {
        theme::system_color(COLOR_GRAYTEXT)
    } else if is_dark {
        Color::from_hex("#3C3C3C")
    } else {
        Color::from_hex("#D8D8D8")
    }
}

/// Create the tray icon: the first available quota percentage on top and zero,
/// one, or two proportional bars below. A single window gets one thicker,
/// centered bar; two windows retain the compact stacked layout. While no data
/// is available, standard themes use the provider tile; High Contrast retains
/// the system-colour company-initial fallback.
pub fn create_icon(
    kind: TrayIconKind,
    percents: &[f64],
    is_dark: bool,
    high_contrast: bool,
) -> HICON {
    if percents.is_empty() && !high_contrast {
        if let Some((icon, size)) =
            provider_tile::create_chip16_icon(kind.brand(), TRAY_BRAND_TILE_DPI, is_dark)
        {
            debug_assert_eq!(size, ICON_SIZE);
            return icon;
        }
    }

    let size = ICON_SIZE;
    let base_col = provider_color(kind, is_dark, high_contrast);
    let percent = percents.first().copied();
    let number_col = match percent {
        Some(p) => number_color(kind, p, is_dark, high_contrast),
        None => base_col,
    };
    let track_col = bar_track_color(is_dark, high_contrast);

    let display_text = match percent {
        Some(p) => compact_view::display_percent(p).to_string(),
        None => placeholder_letter(kind).to_string(),
    };

    let font_h = -42;
    let text_y_offset = if percent.is_some() { -1 } else { 0 };

    unsafe {
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return HICON::default();
        }

        let old_bmp = SelectObject(mem_dc, dib);

        // Zero-fill (transparent background)
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        for px in pixel_data.iter_mut() {
            *px = 0;
        }

        // 1. Number (or placeholder letter) across the upper area.
        // Arial Bold matches jens-duttke/usage-monitor-for-claude's tray digits.
        let font_name = native_interop::wide_str("Arial");
        let font = CreateFontW(
            font_h,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            ANTIALIASED_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(mem_dc, font);
        let _ = SetBkMode(mem_dc, TRANSPARENT);
        let _ = SetTextColor(mem_dc, COLORREF(number_col.to_colorref()));

        let mut text_rect = RECT {
            left: 0,
            top: NUMBER_TOP + text_y_offset,
            right: size,
            bottom: NUMBER_BOTTOM + text_y_offset,
        };
        let mut text_wide: Vec<u16> = display_text.encode_utf16().collect();
        let _ = DrawTextW(
            mem_dc,
            &mut text_wide,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        SelectObject(mem_dc, old_font);
        let _ = DeleteObject(font);

        // 2. Adaptive usage bars.
        let draw_bar = |dc: HDC, top: i32, height: i32, value: f64, fill_col: Color| {
            let track = CreateSolidBrush(COLORREF(track_col.to_colorref()));
            let rect = RECT {
                left: BAR_LEFT,
                top,
                right: BAR_RIGHT,
                bottom: top + height,
            };
            let _ = FillRect(dc, &rect, track);
            let _ = DeleteObject(track);
            let width =
                ((BAR_RIGHT - BAR_LEFT) as f64 * value.clamp(0.0, 100.0) / 100.0).round() as i32;
            if width > 0 {
                let fill = CreateSolidBrush(COLORREF(fill_col.to_colorref()));
                let rect = RECT {
                    left: BAR_LEFT,
                    top,
                    right: BAR_LEFT + width.min(BAR_RIGHT - BAR_LEFT),
                    bottom: top + height,
                };
                let _ = FillRect(dc, &rect, fill);
                let _ = DeleteObject(fill);
            }
        };
        match percents {
            [single] => draw_bar(
                mem_dc,
                SINGLE_BAR_TOP,
                SINGLE_BAR_HEIGHT,
                *single,
                number_color(kind, *single, is_dark, high_contrast),
            ),
            [first, second, ..] => {
                draw_bar(
                    mem_dc,
                    BAR_5H_TOP,
                    BAR_HEIGHT,
                    *first,
                    number_color(kind, *first, is_dark, high_contrast),
                );
                draw_bar(
                    mem_dc,
                    BAR_7D_TOP,
                    BAR_HEIGHT,
                    *second,
                    number_color(kind, *second, is_dark, high_contrast),
                );
            }
            [] => {}
        }

        // Set alpha: non-zero BGR pixel -> fully opaque; background stays transparent
        for px in pixel_data.iter_mut() {
            if *px != 0 {
                *px = (*px & 0x00FF_FFFF) | 0xFF00_0000;
            }
        }

        // Monochrome mask (per-pixel alpha from colour bitmap)
        let mask_bytes = vec![0u8; ((size * size + 7) / 8) as usize];
        let mask_bmp = CreateBitmap(
            size,
            size,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const std::ffi::c_void),
        );

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: dib,
        };
        let hicon = CreateIconIndirect(&icon_info).unwrap_or_default();

        let _ = DeleteObject(mask_bmp);
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        hicon
    }
}

/// How much a balloon is asking of the user. This is the same distinction the
/// detail window already draws between a degraded badge and an action-required
/// one: the system warning glyph is reserved for the notifications that stop
/// monitoring until the user does something, so that seeing it still means
/// something by the time one arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BalloonTone {
    /// Reports something that already happened - a finished CLI update, an
    /// elapsed quota window, a newly detected provider. Carries the app icon
    /// and no sound.
    Info,
    /// Monitoring is stopped until the user acts: a credential the provider
    /// rejected, or one on this machine that cannot be used.
    ActionRequired,
}

/// Balloon image, cached per DPI and kept alive for the process.
///
/// `hBalloonIcon` is not like the tray icon's `hIcon`. The shell copies `hIcon`
/// while `Shell_NotifyIconW` runs, but Windows 10/11 renders a tray balloon as
/// a toast *after* the call returns, so destroying the balloon handle on the
/// way out leaves nothing to draw: the call still reports success and the
/// balloon is silently dropped. Blanket-leaking is not the answer either -
/// `load_embedded_app_icon_of_size` can fall back to `ExtractIconExW`, whose
/// handles we own. Keep one handle per DPI instead.
///
/// Nothing is ever evicted. Evicting on a DPI change would destroy a handle
/// the shell may still be about to draw - the same premature destruction this
/// cache exists to avoid - and a caller reads its handle after releasing the
/// lock, so a concurrent notification at another DPI could pull it out from
/// under that caller. The set is bounded by the number of distinct DPIs the
/// window ever reports, which is a handful.
static BALLOON_ICONS: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());

fn cached_balloon_icon(hwnd: HWND) -> HICON {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi };
    let mut cached = BALLOON_ICONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some((_, handle)) = cached.iter().find(|(cached_dpi, _)| *cached_dpi == dpi) {
        return HICON(*handle as *mut std::ffi::c_void);
    }
    let icon = load_embedded_app_icon_of_size(hwnd, true);
    if !icon.is_invalid() {
        cached.push((dpi, icon.0 as isize));
    }
    icon
}

/// Balloon icon and sound for a tone. `has_custom_icon` reports whether an app
/// icon was actually loaded for `hBalloonIcon`.
fn balloon_info_flags(tone: BalloonTone, has_custom_icon: bool) -> NOTIFY_ICON_INFOTIP_FLAGS {
    match tone {
        BalloonTone::Info if has_custom_icon => NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND,
        // NIIF_USER without a handle falls back to the notification area icon,
        // which here is a live percentage read-out that is unreadable at
        // balloon size. Leave the slot empty rather than show that.
        BalloonTone::Info => NIIF_NONE | NIIF_NOSOUND,
        BalloonTone::ActionRequired => NIIF_WARNING,
    }
}

/// Attach the balloon to one specific tray icon. Fails when that icon is not
/// currently registered, which is how the caller walks to the next candidate.
unsafe fn deliver_balloon(
    mut nid: NOTIFYICONDATAW,
    info_flags: NOTIFY_ICON_INFOTIP_FLAGS,
    balloon_icon: HICON,
    title: &str,
    message: &str,
) -> bool {
    nid.uFlags |= NIF_INFO;
    nid.dwInfoFlags = info_flags;
    nid.hBalloonIcon = balloon_icon;
    copy_wide(title, &mut nid.szInfoTitle);
    copy_wide_256(message, &mut nid.szInfo);
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() }
}

/// What clicking the balloon now on screen offers.
///
/// Kept here because every balloon is raised through this module, which makes
/// it the one place that can guarantee the offer belongs to the balloon the
/// user actually saw: raising any other balloon replaces it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BalloonClick {
    /// Offer this version, the way the update menu entry would.
    Update { version: String },
}

static BALLOON_CLICK: Mutex<Option<BalloonClick>> = Mutex::new(None);

fn set_balloon_click(click: Option<BalloonClick>) {
    *BALLOON_CLICK.lock().unwrap_or_else(|e| e.into_inner()) = click;
}

/// Record what the balloon that just appeared offers, or nothing if it never
/// appeared.
///
/// An offer must not outlive a failed delivery: the user never saw that
/// balloon, and a later click (including from notification history) must not
/// answer a question that was never asked. A failed delivery also drops a
/// previous offer; that balloon is no longer the one on screen.
fn settle_balloon_click(click: Option<BalloonClick>, delivered: bool) {
    set_balloon_click(if delivered { click } else { None });
}

/// Take what the balloon the user just clicked offers, if it offers anything.
///
/// Taking clears it: an offer is answered once. It is deliberately not
/// cleared when the balloon times out: Windows may still deliver a click
/// from notification history (on Windows 11 26200 that was Settings →
/// System → Notifications, not the taskbar notification-centre flyout).
/// That click deserves the same answer as the one on the banner.
pub fn take_balloon_click() -> Option<BalloonClick> {
    BALLOON_CLICK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// Show a Windows balloon notification from a provider's tray icon.
///
/// Clicking it does not start an update; provider news is news, not an offer.
/// Raising it still replaces any standing offer so a click cannot install the
/// previous version.
pub fn notify_balloon(
    hwnd: HWND,
    kind: TrayIconKind,
    tone: BalloonTone,
    title: &str,
    message: &str,
) {
    notify_balloon_anchored(hwnd, Some(kind), tone, title, message, None);
}

/// Show a balloon that belongs to the app rather than to one provider.
///
/// An available update is nobody's provider news, so the app icon is the
/// anchor to try first; the same fallback walk still applies, because
/// detailed mode removes that icon.
pub fn notify_app_balloon(
    hwnd: HWND,
    tone: BalloonTone,
    title: &str,
    message: &str,
    click: Option<BalloonClick>,
) {
    notify_balloon_anchored(hwnd, None, tone, title, message, click);
}

fn notify_balloon_anchored(
    hwnd: HWND,
    kind: Option<TrayIconKind>,
    tone: BalloonTone,
    title: &str,
    message: &str,
    click: Option<BalloonClick>,
) {
    let delivered = unsafe {
        let balloon_icon = match tone {
            BalloonTone::Info => cached_balloon_icon(hwnd),
            BalloonTone::ActionRequired => HICON::default(),
        };
        let info_flags = balloon_info_flags(tone, !balloon_icon.is_invalid());
        // A balloon can only be shown on an icon the tray currently has. The
        // provider's own icon is the right anchor when it exists, but the
        // detection announcement is defined by the provider *not* being shown,
        // so its icon has been removed - and detailed mode removes the app icon
        // too, leaving both of the obvious anchors gone. Fall through to any
        // icon that is actually registered rather than dropping the balloon.
        kind.is_some_and(|kind| {
            deliver_balloon(
                notify_icon_data(hwnd, kind),
                info_flags,
                balloon_icon,
                title,
                message,
            )
        }) || deliver_balloon(
            app_notify_icon_data(hwnd),
            info_flags,
            balloon_icon,
            title,
            message,
        ) || TrayIconKind::ALL.into_iter().any(|other| {
            Some(other) != kind
                && deliver_balloon(
                    notify_icon_data(hwnd, other),
                    info_flags,
                    balloon_icon,
                    title,
                    message,
                )
        })
    };
    if !delivered {
        diagnose::log(format!(
            "balloon not delivered; no registered tray icon kind={}",
            kind.map_or("app", TrayIconKind::diagnostic_label)
        ));
    }
    // After delivery, not before: an offer exists only for a balloon the
    // user actually saw. A click on this balloon can never answer the
    // previous one's question, including when this one never appeared.
    settle_balloon_click(click, delivered);
}

fn notify_icon_data_for_mode(
    hwnd: HWND,
    kind: TrayIconKind,
    mode: IdentityMode,
) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: kind.id(),
        ..Default::default()
    };
    if mode == IdentityMode::Guid {
        nid.uFlags |= NIF_GUID;
        nid.guidItem = kind.guid();
    }
    nid
}

fn notify_icon_data(hwnd: HWND, kind: TrayIconKind) -> NOTIFYICONDATAW {
    notify_icon_data_for_mode(hwnd, kind, kind.identity_mode())
}

fn app_notify_icon_data_for_mode(hwnd: HWND, mode: IdentityMode) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: APP_TRAY_ICON_ID,
        ..Default::default()
    };
    if mode == IdentityMode::Guid {
        nid.uFlags |= NIF_GUID;
        nid.guidItem = app_icon_guid();
    }
    nid
}

fn app_notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    app_notify_icon_data_for_mode(hwnd, app_identity_mode())
}

fn provider_tooltip_notify_icon_data_for_mode(
    hwnd: HWND,
    kind: TrayIconKind,
    mode: IdentityMode,
    tooltip: &str,
) -> NOTIFYICONDATAW {
    let mut nid = notify_icon_data_for_mode(hwnd, kind, mode);
    nid.uFlags |= NIF_TIP | NIF_SHOWTIP;
    copy_to_tip(tooltip, &mut nid.szTip);
    nid
}

fn update_provider_tooltip(hwnd: HWND, kind: TrayIconKind, tooltip: &str) {
    unsafe {
        let nid =
            provider_tooltip_notify_icon_data_for_mode(hwnd, kind, kind.identity_mode(), tooltip);
        // Countdown-only refresh: modifying the tooltip must not recreate the
        // HICON, re-register the callback, or disturb the user's Shell order.
        //
        // Not logged: this fails while the shell is between restarts, which is
        // already reported by the taskbar watchdog, and the only consequence
        // is a tooltip that stays stale until the next `sync` rewrites it.
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Refresh the native provider tooltip text without touching icon identity,
/// artwork, callback registration, or Shell ordering.
pub fn update_provider_tooltips(hwnd: HWND, icons: &[TrayIconData]) {
    for icon in icons {
        update_provider_tooltip(hwnd, icon.kind, &icon.tooltip);
    }
}

/// Copy a string into a fixed-size wide buffer (truncates to fit).
fn copy_wide<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let len = wide.len().min(N - 1);
    buf[..len].copy_from_slice(&wide[..len]);
    buf[len] = 0;
}

/// Copy a string into a 256-wide buffer.
fn copy_wide_256(s: &str, buf: &mut [u16; 256]) {
    copy_wide(s, buf)
}

/// Register or refresh the tray icon with the shell: try NIM_MODIFY first
/// (the common case on every poll) and fall back to NIM_ADD when the icon is
/// not registered - a fresh start, or explorer restarted and dropped every
/// tray registration. One icon render either way.
pub fn ensure(hwnd: HWND, kind: TrayIconKind, percents: &[f64], tooltip: &str) {
    let hicon = create_icon(
        kind,
        percents,
        theme::is_dark_mode(),
        theme::is_high_contrast(),
    );
    unsafe {
        let mut nid = notify_icon_data(hwnd, kind);
        nid.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            nid.uFlags |= NIF_MESSAGE;
            nid.uCallbackMessage = WM_APP_TRAY;
            let mut added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
            if !added && kind.identity_mode() == IdentityMode::Guid {
                let mut fallback = notify_icon_data_for_mode(hwnd, kind, IdentityMode::LegacyUId);
                fallback.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
                fallback.uCallbackMessage = WM_APP_TRAY;
                fallback.hIcon = hicon;
                copy_to_tip(tooltip, &mut fallback.szTip);
                if Shell_NotifyIconW(NIM_ADD, &fallback).as_bool() {
                    kind.use_legacy_uid();
                    nid = fallback;
                    added = true;
                    diagnose::log(format!(
                        "tray GUID registration failed; using legacy uID kind={kind:?}"
                    ));
                }
            }
            if added {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                if !Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool() {
                    diagnose::log(format!(
                        "tray icon version negotiation failed kind={kind:?}"
                    ));
                }
            } else {
                diagnose::log(format!("tray icon registration failed kind={kind:?}"));
            }
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

fn load_embedded_app_icon(hwnd: HWND) -> HICON {
    load_embedded_app_icon_of_size(hwnd, false)
}

/// `want_large` asks for the 32px-class icon a balloon's custom icon slot
/// expects; the notification area itself wants the small one.
fn load_embedded_app_icon_of_size(hwnd: HWND, want_large: bool) -> HICON {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let dpi = if dpi == 0 { 96 } else { dpi };
        let (cx, cy) = if want_large {
            (SM_CXICON, SM_CYICON)
        } else {
            (SM_CXSMICON, SM_CYSMICON)
        };
        let width = GetSystemMetricsForDpi(cx, dpi);
        let height = GetSystemMetricsForDpi(cy, dpi);
        if width > 0 && height > 0 {
            if let Ok(module) = GetModuleHandleW(PCWSTR::null()) {
                let resource = PCWSTR::from_raw(APP_ICON_RESOURCE_ID as *const u16);
                if let Ok(icon) = LoadImageW(
                    HINSTANCE(module.0),
                    resource,
                    IMAGE_ICON,
                    width,
                    height,
                    LR_DEFAULTCOLOR,
                ) {
                    return HICON(icon.0);
                }
            }
        }

        let mut exe_buf = [0u16; 260];
        if GetModuleFileNameW(None, &mut exe_buf) == 0 {
            return HICON::default();
        }

        let mut large = HICON::default();
        let mut small = HICON::default();
        if ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large),
            Some(&mut small),
            1,
        ) == 0
        {
            return HICON::default();
        }
        let (preferred, other) = if want_large {
            (large, small)
        } else {
            (small, large)
        };
        if !preferred.is_invalid() {
            if !other.is_invalid() {
                let _ = DestroyIcon(other);
            }
            preferred
        } else {
            other
        }
    }
}

fn ensure_app(hwnd: HWND, tooltip: &str) {
    let hicon = load_embedded_app_icon(hwnd);
    unsafe {
        let mut nid = app_notify_icon_data(hwnd);
        nid.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            nid.uFlags |= NIF_MESSAGE;
            nid.uCallbackMessage = WM_APP_TRAY;
            let mut added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
            if !added && app_identity_mode() == IdentityMode::Guid {
                let mut fallback = app_notify_icon_data_for_mode(hwnd, IdentityMode::LegacyUId);
                fallback.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP | NIF_MESSAGE;
                fallback.uCallbackMessage = WM_APP_TRAY;
                fallback.hIcon = hicon;
                copy_to_tip(tooltip, &mut fallback.szTip);
                if Shell_NotifyIconW(NIM_ADD, &fallback).as_bool() {
                    use_legacy_app_uid();
                    nid = fallback;
                    added = true;
                    diagnose::log("app tray GUID registration failed; using legacy uID");
                }
            }
            if added {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                if !Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool() {
                    diagnose::log("app tray icon version negotiation failed");
                }
            } else {
                diagnose::log("app tray icon registration failed");
            }
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Remove the tray icon from the shell.
///
/// The failure is deliberately not logged. `sync` calls this for every
/// provider that is not currently shown, most of which were never registered,
/// so `NIM_DELETE` failing is the normal path rather than a symptom - logging
/// it would bury real diagnostics under one line per hidden provider per
/// refresh.
pub fn remove(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let nid = notify_icon_data(hwnd, kind);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Return keyboard focus to the notification area after a tray context menu,
/// as required by the Shell notification icon contract.
pub fn restore_focus(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let nid = notify_icon_data(hwnd, kind);
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &nid);
    }
}

pub fn restore_app_focus(hwnd: HWND) {
    unsafe {
        let nid = app_notify_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &nid);
    }
}

/// Remove the application icon. Fails routinely for the same reason as
/// [`remove`]: per-provider icon mode calls it whether or not it was added.
fn remove_app(hwnd: HWND) {
    unsafe {
        let nid = app_notify_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

pub fn sync(hwnd: HWND, icons: &[TrayIconData], detailed_icons: bool, app_tooltip: &str) {
    if !detailed_icons {
        ensure_app(hwnd, app_tooltip);
        for kind in TrayIconKind::ALL {
            remove(hwnd, kind);
        }
        return;
    }

    for kind in TrayIconKind::ALL {
        match icons.iter().find(|icon| icon.kind.id() == kind.id()) {
            Some(icon) => ensure(hwnd, icon.kind, &icon.percents, &icon.tooltip),
            None => remove(hwnd, kind),
        }
    }
    remove_app(hwnd);
}

pub fn remove_all(hwnd: HWND) {
    for kind in TrayIconKind::ALL {
        remove(hwnd, kind);
    }
    remove_app(hwnd);
}

/// Render every icon state to 32bpp BMP files for offline visual review
/// (`--dump-tray-icons <dir>`). Returns a process exit code.
pub fn dump_icons(dir: &str) -> i32 {
    let cases: &[(TrayIconKind, &str, &[f64])] = &[
        (TrayIconKind::Claude, "claude-nodata", &[]),
        (TrayIconKind::Claude, "claude-single-35", &[35.0]),
        (TrayIconKind::Claude, "claude-72-48", &[72.0, 48.0]),
        (TrayIconKind::Claude, "claude-95-88", &[95.0, 88.0]),
        (TrayIconKind::Codex, "codex-nodata", &[]),
        (TrayIconKind::Codex, "codex-single-1", &[1.0]),
        (TrayIconKind::Codex, "codex-42-12", &[42.0, 12.0]),
        (TrayIconKind::Antigravity, "ag-nodata", &[]),
        (TrayIconKind::Antigravity, "ag-single-60", &[60.0]),
        (TrayIconKind::Antigravity, "ag-100-95", &[100.0, 95.0]),
        (TrayIconKind::Grok, "grok-nodata", &[]),
        (TrayIconKind::Grok, "grok-single-23", &[23.0]),
        (TrayIconKind::Grok, "grok-91", &[91.0]),
    ];
    if std::fs::create_dir_all(dir).is_err() {
        return 1;
    }
    let mut failures = 0;
    for (kind, name, percents) in cases {
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            let hicon = create_icon(*kind, percents, is_dark, false);
            let path = format!("{dir}\\{name}-{theme_name}.bmp");
            if hicon.is_invalid() || !icon_to_bmp(hicon, &path) {
                failures += 1;
            }
            if !hicon.is_invalid() {
                unsafe {
                    let _ = DestroyIcon(hicon);
                }
            }
        }
        if percents.is_empty() {
            let hicon = create_icon(*kind, percents, false, true);
            let path = format!("{dir}\\{name}-hc.bmp");
            if hicon.is_invalid() || !icon_to_bmp(hicon, &path) {
                failures += 1;
            }
            if !hicon.is_invalid() {
                unsafe {
                    let _ = DestroyIcon(hicon);
                }
            }
        }
    }
    if failures == 0 {
        0
    } else {
        1
    }
}

// Kept beside the Shell-ordering helpers; bitmap export and callback helpers
// below are production code rather than additional test-only items.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn provider_identity_mode_preserves_uid_and_only_guid_mode_sets_nif_guid() {
        let guid =
            notify_icon_data_for_mode(HWND::default(), TrayIconKind::Claude, IdentityMode::Guid);
        let legacy = notify_icon_data_for_mode(
            HWND::default(),
            TrayIconKind::Claude,
            IdentityMode::LegacyUId,
        );

        assert_eq!(guid.uID, CLAUDE_TRAY_ICON_ID);
        assert_eq!(legacy.uID, CLAUDE_TRAY_ICON_ID);
        assert_ne!(guid.uFlags.0 & NIF_GUID.0, 0);
        assert_eq!(legacy.uFlags.0 & NIF_GUID.0, 0);
        assert_eq!(legacy.guidItem, GUID::zeroed());
    }

    #[test]
    fn app_identity_mode_preserves_uid_and_only_guid_mode_sets_nif_guid() {
        let guid = app_notify_icon_data_for_mode(HWND::default(), IdentityMode::Guid);
        let legacy = app_notify_icon_data_for_mode(HWND::default(), IdentityMode::LegacyUId);

        assert_eq!(guid.uID, APP_TRAY_ICON_ID);
        assert_eq!(legacy.uID, APP_TRAY_ICON_ID);
        assert_ne!(guid.uFlags.0 & NIF_GUID.0, 0);
        assert_eq!(legacy.uFlags.0 & NIF_GUID.0, 0);
        assert_eq!(legacy.guidItem, GUID::zeroed());
    }

    #[test]
    fn tray_number_warning_color_follows_the_displayed_percentage() {
        let normal = number_color(TrayIconKind::Claude, 89.4, false, false);
        let rounded_warning = number_color(TrayIconKind::Claude, 89.6, false, false);

        assert_eq!((normal.r, normal.g, normal.b), (0xD9, 0x77, 0x57));
        assert_eq!(
            (rounded_warning.r, rounded_warning.g, rounded_warning.b),
            (0xE5, 0x48, 0x4D)
        );
    }

    /// The warning glyph is the app's only "you have to do something" marker
    /// in the notification area. Routine disclosures must not spend it, or it
    /// stops meaning anything by the time a credential actually fails.
    #[test]
    fn only_action_required_balloons_carry_the_system_warning_glyph() {
        assert_eq!(
            balloon_info_flags(BalloonTone::ActionRequired, false),
            NIIF_WARNING
        );
        assert_eq!(
            balloon_info_flags(BalloonTone::ActionRequired, true),
            NIIF_WARNING
        );
        assert_eq!(
            balloon_info_flags(BalloonTone::Info, true),
            NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND
        );
        assert_eq!(
            balloon_info_flags(BalloonTone::Info, false),
            NIIF_NONE | NIIF_NOSOUND
        );
    }

    #[test]
    fn countdown_tooltip_refresh_does_not_modify_icon_or_callback() {
        let nid = provider_tooltip_notify_icon_data_for_mode(
            HWND::default(),
            TrayIconKind::Claude,
            IdentityMode::Guid,
            "Claude Code\n5h: 53% (Resets in 45m)",
        );

        assert_ne!(nid.uFlags.0 & NIF_GUID.0, 0);
        assert_ne!(nid.uFlags.0 & NIF_TIP.0, 0);
        assert_ne!(nid.uFlags.0 & NIF_SHOWTIP.0, 0);
        assert_eq!(nid.uFlags.0 & NIF_ICON.0, 0);
        assert_eq!(nid.uFlags.0 & NIF_MESSAGE.0, 0);
        assert!(nid.hIcon.is_invalid());
        assert_eq!(nid.uCallbackMessage, 0);
    }

    #[test]
    fn orders_single_row_icons_left_to_right() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1810, 1050, 1826, 1066)),
            (TrayIconKind::Antigravity, rect(1830, 1050, 1846, 1066)),
        ];

        assert_eq!(
            order_from_rects(&positions, &taskbar),
            Some(vec![
                TrayIconKind::Codex,
                TrayIconKind::Antigravity,
                TrayIconKind::Claude,
            ])
        );
    }

    #[test]
    fn orders_vertical_taskbar_icons_top_to_bottom() {
        let taskbar = rect(0, 0, 48, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(12, 1020, 28, 1036)),
            (TrayIconKind::Codex, rect(12, 980, 28, 996)),
        ];

        assert_eq!(
            order_from_rects(&positions, &taskbar),
            Some(vec![TrayIconKind::Codex, TrayIconKind::Claude])
        );
    }

    #[test]
    fn rejects_overflow_or_other_taskbar_rectangles() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1600, 900, 1616, 916)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn rejects_ambiguous_multi_row_layout() {
        let taskbar = rect(0, 1000, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1010, 1866, 1026)),
            (TrayIconKind::Codex, rect(1830, 1050, 1846, 1066)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn rejects_duplicate_shell_locations() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1850, 1050, 1866, 1066)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn icon_guids_are_stable_and_unique() {
        // Built from `ALL` rather than spelled out: the hand-written list
        // silently stopped covering every provider the moment a fourth one
        // was added, which is exactly what this test exists to prevent.
        let mut guids = vec![app_icon_guid()];
        guids.extend(TrayIconKind::ALL.into_iter().map(TrayIconKind::guid));
        assert_eq!(guids.len(), TrayIconKind::COUNT + 1);
        for (index, guid) in guids.iter().enumerate() {
            for other in &guids[index + 1..] {
                assert_ne!(guid, other, "duplicate tray icon GUID");
            }
        }
    }

    /// A balloon is delivered to whichever icon the tray happens to have, so
    /// the click must be recognised whatever icon id it arrives on, and it
    /// must not be mistaken for a click on the icon itself.
    #[test]
    fn a_balloon_click_is_recognised_on_any_icon() {
        for id in [APP_TRAY_ICON_ID, TrayIconKind::Grok.id()] {
            let clicked = LPARAM(((id << 16) | NIN_BALLOONUSERCLICK) as usize as isize);
            assert_eq!(
                handle_message(clicked),
                TrayAction::BalloonClicked,
                "id {id}"
            );
        }
    }

    /// An offer is answered once, and only by the balloon that made it: the
    /// next balloon to appear replaces it, so a click on a quota-reset or
    /// provider-detection balloon cannot act on the previous update version.
    #[test]
    fn only_the_balloon_on_screen_carries_an_offer() {
        set_balloon_click(Some(BalloonClick::Update {
            version: "9.9.9".to_string(),
        }));
        set_balloon_click(None);
        assert_eq!(take_balloon_click(), None, "a later balloon replaced it");

        set_balloon_click(Some(BalloonClick::Update {
            version: "9.9.9".to_string(),
        }));
        assert_eq!(
            take_balloon_click(),
            Some(BalloonClick::Update {
                version: "9.9.9".to_string(),
            })
        );
        assert_eq!(take_balloon_click(), None, "taking answers it once");
    }

    /// A balloon that `Shell_NotifyIconW` never accepted must not leave an
    /// offer behind, including an offer from the balloon it failed to replace.
    #[test]
    fn a_balloon_that_never_appeared_carries_no_offer() {
        set_balloon_click(Some(BalloonClick::Update {
            version: "9.9.8".to_string(),
        }));
        settle_balloon_click(
            Some(BalloonClick::Update {
                version: "9.9.9".to_string(),
            }),
            false,
        );
        assert_eq!(
            take_balloon_click(),
            None,
            "failed delivery leaves no offer"
        );

        settle_balloon_click(
            Some(BalloonClick::Update {
                version: "9.9.9".to_string(),
            }),
            true,
        );
        assert_eq!(
            take_balloon_click(),
            Some(BalloonClick::Update {
                version: "9.9.9".to_string(),
            })
        );

        settle_balloon_click(None, true);
        assert_eq!(
            take_balloon_click(),
            None,
            "a delivered news balloon replaces the offer"
        );
    }

    #[test]
    fn version_four_callback_decodes_icon_and_keyboard_events() {
        let select = LPARAM(((TrayIconKind::Codex.id() << 16) | NIN_KEYSELECT) as usize as isize);
        assert_eq!(
            handle_message(select),
            TrayAction::ShowDetails {
                kind: Some(TrayIconKind::Codex),
                keyboard: true,
            }
        );

        let pointer = LPARAM(((TrayIconKind::Claude.id() << 16) | NIN_SELECT) as usize as isize);
        assert_eq!(
            handle_message(pointer),
            TrayAction::ShowDetails {
                kind: Some(TrayIconKind::Claude),
                keyboard: false,
            }
        );

        let app_keyboard = LPARAM(((APP_TRAY_ICON_ID << 16) | NIN_KEYSELECT) as usize as isize);
        assert_eq!(
            handle_message(app_keyboard),
            TrayAction::ShowDetails {
                kind: None,
                keyboard: true,
            }
        );

        let context =
            LPARAM(((TrayIconKind::Antigravity.id() << 16) | WM_CONTEXTMENU) as usize as isize);
        assert!(matches!(
            handle_message(context),
            TrayAction::ShowContextMenu {
                kind: Some(TrayIconKind::Antigravity),
                anchor_to_icon: true,
            }
        ));

        let app_context = LPARAM(((APP_TRAY_ICON_ID << 16) | WM_CONTEXTMENU) as usize as isize);
        assert!(matches!(
            handle_message(app_context),
            TrayAction::ShowContextMenu {
                kind: None,
                anchor_to_icon: true,
            }
        ));
    }

    #[test]
    fn tray_geometry_keeps_text_and_bars_in_bounds() {
        let geometry = [
            ICON_SIZE,
            NUMBER_TOP,
            NUMBER_BOTTOM,
            BAR_LEFT,
            BAR_RIGHT,
            BAR_5H_TOP,
            BAR_7D_TOP,
            BAR_HEIGHT,
            SINGLE_BAR_TOP,
            SINGLE_BAR_HEIGHT,
        ];
        let [size, number_top, number_bottom, bar_left, bar_right, first_top, second_top, bar_height, single_top, single_height] =
            geometry;
        assert!(number_top >= 0);
        assert!(number_bottom <= first_top);
        assert!(first_top + bar_height <= second_top);
        assert!(second_top + bar_height <= size);
        assert!(number_bottom <= single_top);
        assert!(single_top + single_height <= size);
        assert!(bar_left >= 0 && bar_right <= size);
    }
}

fn icon_to_bmp(hicon: HICON, path: &str) -> bool {
    const SIZE: i32 = ICON_SIZE;
    unsafe {
        let mut info = ICONINFO::default();
        if GetIconInfo(hicon, &mut info).is_err() {
            return false;
        }
        let color_bmp = info.hbmColor;

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
        let screen_dc = GetDC(HWND::default());
        let rows = GetDIBits(
            screen_dc,
            color_bmp,
            0,
            SIZE as u32,
            Some(pixels.as_mut_ptr() as *mut std::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(HWND::default(), screen_dc);
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
        if rows == 0 {
            return false;
        }

        // Minimal 32bpp BMP: file header + top-down info header + BGRA pixels.
        let pixel_bytes = pixels.len() as u32;
        let mut file = Vec::with_capacity(54 + pixels.len());
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + pixel_bytes).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&SIZE.to_le_bytes());
        file.extend_from_slice(&(-SIZE).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixel_bytes.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);
        std::fs::write(path, file).is_ok()
    }
}

/// Interpret a tray callback message and return the action to take.
pub fn handle_message(lparam: LPARAM) -> TrayAction {
    let raw = lparam.0 as u32;
    let event = raw & 0xFFFF;
    let kind = TrayIconKind::from_id(raw >> 16);
    match event {
        WM_LBUTTONUP | NIN_SELECT => TrayAction::ShowDetails {
            kind,
            keyboard: false,
        },
        NIN_KEYSELECT => TrayAction::ShowDetails {
            kind,
            keyboard: true,
        },
        WM_RBUTTONUP => TrayAction::ShowContextMenu {
            kind,
            anchor_to_icon: false,
        },
        WM_CONTEXTMENU => TrayAction::ShowContextMenu {
            kind,
            anchor_to_icon: true,
        },
        NIN_BALLOONUSERCLICK => TrayAction::BalloonClicked,
        _ => TrayAction::None,
    }
}

/// Copy a string into the fixed-size szTip field (max 127 chars + null).
fn copy_to_tip(s: &str, tip: &mut [u16; 128]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut len = wide.len().min(127);
    // Don't leave a lone high surrogate at the truncation point
    if len > 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
        len -= 1;
    }
    tip[..len].copy_from_slice(&wide[..len]);
    tip[len] = 0;
}
