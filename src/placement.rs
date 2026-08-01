//! Pure, DPI-aware placement rules for persistent window anchors.

use crate::settings::{
    FloatingPlacement, HorizontalAnchor, VerticalAnchor, WidgetAnchor, WidgetPlacement,
};

pub(crate) const EDGE_MARGIN_DIP: i32 = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PlacementRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub(crate) fn scale_dip(value: i32, dpi: u32) -> i32 {
    ((value as f64) * (dpi.max(1) as f64) / 96.0).round() as i32
}

pub(crate) fn unscale_px(value: i32, dpi: u32) -> i32 {
    ((value as f64) * 96.0 / (dpi.max(1) as f64)).round() as i32
}

pub(crate) fn resolve_widget_x(
    placement: &WidgetPlacement,
    taskbar_left: i32,
    tray_left: i32,
    widget_width: i32,
    dpi: u32,
) -> i32 {
    let tray_left_local = tray_left - taskbar_left;
    match placement {
        WidgetPlacement::PrimaryLeft => scale_dip(EDGE_MARGIN_DIP, dpi),
        WidgetPlacement::PrimaryRight => (tray_left_local - widget_width).max(0),
        WidgetPlacement::Custom {
            anchor, gap_dip, ..
        } => {
            let gap = scale_dip((*gap_dip).max(0), dpi);
            match anchor {
                WidgetAnchor::TaskbarLeft => gap,
                WidgetAnchor::NotificationArea => (tray_left_local - widget_width - gap).max(0),
            }
        }
    }
}

pub(crate) fn custom_widget_anchor(
    taskbar_left: i32,
    tray_left: i32,
    widget_left: i32,
    widget_width: i32,
    dpi: u32,
) -> (WidgetAnchor, i32) {
    let left_gap = (widget_left - taskbar_left).max(0);
    let notification_gap = (tray_left - widget_left - widget_width).max(0);
    if left_gap <= notification_gap {
        (WidgetAnchor::TaskbarLeft, unscale_px(left_gap, dpi))
    } else {
        (
            WidgetAnchor::NotificationArea,
            unscale_px(notification_gap, dpi),
        )
    }
}

pub(crate) fn resolve_floating_rect(
    placement: &FloatingPlacement,
    work: PlacementRect,
    width: i32,
    height: i32,
    dpi: u32,
) -> PlacementRect {
    let (horizontal, vertical, horizontal_gap_dip, vertical_gap_dip) = match placement {
        FloatingPlacement::PrimaryBottomLeft => (
            HorizontalAnchor::Left,
            VerticalAnchor::Bottom,
            EDGE_MARGIN_DIP,
            EDGE_MARGIN_DIP,
        ),
        FloatingPlacement::PrimaryBottomRight => (
            HorizontalAnchor::Right,
            VerticalAnchor::Bottom,
            EDGE_MARGIN_DIP,
            EDGE_MARGIN_DIP,
        ),
        FloatingPlacement::Custom {
            horizontal_anchor,
            vertical_anchor,
            horizontal_gap_dip,
            vertical_gap_dip,
            ..
        } => (
            *horizontal_anchor,
            *vertical_anchor,
            (*horizontal_gap_dip).max(0),
            (*vertical_gap_dip).max(0),
        ),
    };
    let horizontal_gap = scale_dip(horizontal_gap_dip, dpi);
    let vertical_gap = scale_dip(vertical_gap_dip, dpi);
    let left = match horizontal {
        HorizontalAnchor::Left => work.left + horizontal_gap,
        HorizontalAnchor::Right => work.right - width - horizontal_gap,
    };
    let top = match vertical {
        VerticalAnchor::Top => work.top + vertical_gap,
        VerticalAnchor::Bottom => work.bottom - height - vertical_gap,
    };
    let max_left = work.right - width;
    let max_top = work.bottom - height;
    let left = if max_left < work.left {
        work.left
    } else {
        left.clamp(work.left, max_left)
    };
    let top = if max_top < work.top {
        work.top
    } else {
        top.clamp(work.top, max_top)
    };
    PlacementRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

pub(crate) fn custom_floating_anchors(
    work: PlacementRect,
    rect: PlacementRect,
    dpi: u32,
) -> (HorizontalAnchor, VerticalAnchor, i32, i32) {
    let left_gap = (rect.left - work.left).max(0);
    let right_gap = (work.right - rect.right).max(0);
    let top_gap = (rect.top - work.top).max(0);
    let bottom_gap = (work.bottom - rect.bottom).max(0);
    let (horizontal, horizontal_gap) = if left_gap <= right_gap {
        (HorizontalAnchor::Left, left_gap)
    } else {
        (HorizontalAnchor::Right, right_gap)
    };
    let (vertical, vertical_gap) = if top_gap <= bottom_gap {
        (VerticalAnchor::Top, top_gap)
    } else {
        (VerticalAnchor::Bottom, bottom_gap)
    };
    (
        horizontal,
        vertical,
        unscale_px(horizontal_gap, dpi),
        unscale_px(vertical_gap, dpi),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MonitorKey;

    fn monitor() -> MonitorKey {
        MonitorKey {
            device_path: Some("monitor-path".to_string()),
            gdi_device_name: "DISPLAY1".to_string(),
        }
    }

    #[test]
    fn primary_left_ignores_tray_geometry_and_widget_width() {
        let placement = WidgetPlacement::PrimaryLeft;
        for (dpi, expected_margin) in [(96, 8), (120, 10), (144, 12)] {
            assert_eq!(
                resolve_widget_x(&placement, 0, 2_012, 380, dpi),
                expected_margin
            );
            assert_eq!(
                resolve_widget_x(&placement, 0, 2_132, 415, dpi),
                expected_margin
            );
        }
    }

    #[test]
    fn primary_right_preserves_the_notification_area_edge() {
        let placement = WidgetPlacement::PrimaryRight;
        let first = resolve_widget_x(&placement, 0, 2_012, 380, 120);
        let second = resolve_widget_x(&placement, 0, 2_132, 415, 120);
        assert_eq!(first + 380, 2_012);
        assert_eq!(second + 415, 2_132);
    }

    #[test]
    fn custom_widget_anchor_round_trips_at_negative_taskbar_coordinates() {
        let (anchor, gap_dip) = custom_widget_anchor(-1_920, -200, -1_790, 380, 120);
        let placement = WidgetPlacement::Custom {
            monitor: monitor(),
            anchor,
            gap_dip,
        };
        assert_eq!(resolve_widget_x(&placement, -1_920, -200, 380, 120), 130);
    }

    #[test]
    fn custom_notification_anchor_preserves_its_edge_when_geometry_changes() {
        let placement = WidgetPlacement::Custom {
            monitor: monitor(),
            anchor: WidgetAnchor::NotificationArea,
            gap_dip: 16,
        };
        for (tray_left, width) in [(2_012, 380), (2_132, 415)] {
            let left = resolve_widget_x(&placement, 0, tray_left, width, 120);
            assert_eq!(tray_left - (left + width), 20);
        }
    }

    #[test]
    fn floating_presets_keep_their_named_edges_when_size_changes() {
        let work = PlacementRect {
            left: 0,
            top: 0,
            right: 1_920,
            bottom: 1_040,
        };
        for (width, height) in [(180, 52), (260, 88)] {
            let left = resolve_floating_rect(
                &FloatingPlacement::PrimaryBottomLeft,
                work,
                width,
                height,
                96,
            );
            assert_eq!(left.left, 8);
            assert_eq!(work.bottom - left.bottom, 8);

            let right = resolve_floating_rect(
                &FloatingPlacement::PrimaryBottomRight,
                work,
                width,
                height,
                96,
            );
            assert_eq!(work.right - right.right, 8);
            assert_eq!(work.bottom - right.bottom, 8);
        }
    }

    #[test]
    fn custom_floating_anchor_round_trips() {
        let work = PlacementRect {
            left: -1_920,
            top: 0,
            right: 0,
            bottom: 1_040,
        };
        let rect = PlacementRect {
            left: -420,
            top: 800,
            right: -180,
            bottom: 980,
        };
        let (horizontal_anchor, vertical_anchor, horizontal_gap_dip, vertical_gap_dip) =
            custom_floating_anchors(work, rect, 96);
        let placement = FloatingPlacement::Custom {
            monitor: monitor(),
            horizontal_anchor,
            vertical_anchor,
            horizontal_gap_dip,
            vertical_gap_dip,
        };
        assert_eq!(resolve_floating_rect(&placement, work, 240, 180, 96), rect);
    }

    #[test]
    fn custom_floating_right_bottom_edges_survive_content_resize() {
        let work = PlacementRect {
            left: 0,
            top: 0,
            right: 2_560,
            bottom: 1_400,
        };
        let placement = FloatingPlacement::Custom {
            monitor: monitor(),
            horizontal_anchor: HorizontalAnchor::Right,
            vertical_anchor: VerticalAnchor::Bottom,
            horizontal_gap_dip: 24,
            vertical_gap_dip: 16,
        };
        for (width, height) in [(180, 52), (360, 104)] {
            let rect = resolve_floating_rect(&placement, work, width, height, 144);
            assert_eq!(work.right - rect.right, 36);
            assert_eq!(work.bottom - rect.bottom, 24);
        }
    }

    #[test]
    fn oversized_floating_window_is_clamped_to_the_work_area_origin() {
        let work = PlacementRect {
            left: -1_280,
            top: 0,
            right: 0,
            bottom: 720,
        };
        let rect =
            resolve_floating_rect(&FloatingPlacement::PrimaryBottomRight, work, 1_500, 900, 96);
        assert_eq!((rect.left, rect.top), (work.left, work.top));
    }
}
