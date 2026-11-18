//! Where the tray popover appears.
//!
//! Tauri reports tray events in physical pixels. On a Retina display that is
//! twice the coordinate space [`WebviewWindow::set_position`] works in when it
//! is given a [`LogicalPosition`], so both the offset and the size have to be
//! converted before the popover is placed, or it lands off-centre and over the
//! menu bar.

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, PhysicalPosition};

/// Centres the popover under the tray icon, and keeps it inside the screen's
/// work area: below the menu bar, and never off an edge.
pub fn place(
    centre: LogicalPosition<f64>,
    size: LogicalSize<f64>,
    work_area: Option<(LogicalPosition<f64>, LogicalSize<f64>)>,
) -> LogicalPosition<f64> {
    let mut x = centre.x - size.width / 2.0;
    let mut y = centre.y;
    if let Some((origin, area)) = work_area {
        y = y.max(origin.y);
        x = x.clamp(origin.x, (origin.x + area.width - size.width).max(origin.x));
        y = y.min((origin.y + area.height - size.height).max(origin.y));
    }
    LogicalPosition::new(x.round(), y.round())
}

/// Places and shows the popover for a tray click at `point` (physical pixels).
pub fn show(app: &AppHandle, point: PhysicalPosition<f64>) {
    let Some(window) = app.get_webview_window("popover") else {
        return;
    };
    let monitor = window.monitor_from_point(point.x, point.y).ok().flatten();
    let scale = monitor.as_ref().map_or_else(
        || window.scale_factor().unwrap_or(1.0),
        tauri::Monitor::scale_factor,
    );
    let size = window
        .outer_size()
        .map_or(LogicalSize::new(320.0, 300.0), |size| {
            size.to_logical(scale)
        });
    let work_area = monitor.map(|monitor| {
        let area = monitor.work_area();
        (area.position.to_logical(scale), area.size.to_logical(scale))
    });
    let _ = window.set_position(place(point.to_logical(scale), size, work_area));
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_area() -> Option<(LogicalPosition<f64>, LogicalSize<f64>)> {
        Some((
            LogicalPosition::new(0.0, 25.0),
            LogicalSize::new(1440.0, 875.0),
        ))
    }

    #[test]
    fn centres_under_the_icon_and_below_the_menu_bar() {
        // A Retina click at (2400, 24) physical is (1200, 12) logical.
        let placed = place(
            PhysicalPosition::new(2400.0, 24.0).to_logical::<f64>(2.0),
            LogicalSize::new(320.0, 300.0),
            work_area(),
        );
        assert_eq!(placed, LogicalPosition::new(1040.0, 25.0));
    }

    #[test]
    fn stays_inside_the_work_area_at_the_screen_edges() {
        let size = LogicalSize::new(320.0, 300.0);
        let right = place(LogicalPosition::new(1435.0, 30.0), size, work_area());
        assert_eq!(right.x, 1440.0 - 320.0);
        let left = place(LogicalPosition::new(4.0, 30.0), size, work_area());
        assert_eq!(left.x, 0.0);
    }
}
