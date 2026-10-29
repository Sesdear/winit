//! Small coordinate conversions shared by the native file-drop bridges.

/// Converts surface-local logical points to physical pixels with one scale application.
/// Rejects invalid scales, non-finite input and coordinates that overflow during scaling.
#[cfg(any(wayland_platform, macos_platform, test))]
pub(super) fn logical_position(
    x: f64,
    y: f64,
    scale: f64,
) -> Option<crate::dpi::PhysicalPosition<f64>> {
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let position = crate::dpi::LogicalPosition::new(x, y).to_physical::<f64>(scale);
    (position.x.is_finite() && position.y.is_finite()).then_some(position)
}

/// Converts physical screen pixels into client pixels without applying DPI scaling again.
/// Wide subtraction preserves negative monitor origins and extreme signed coordinates.
#[cfg(any(windows_platform, test))]
pub(super) fn screen_position(
    point: (i32, i32),
    origin: (i32, i32),
) -> crate::dpi::PhysicalPosition<f64> {
    crate::dpi::PhysicalPosition::new(
        (i64::from(point.0) - i64::from(origin.0)) as f64,
        (i64::from(point.1) - i64::from(origin.1)) as f64,
    )
}

/// Unpacks XDND root coordinates as signed 16-bit pixels, preserving negative monitor positions.
#[cfg(any(x11_platform, test))]
pub(super) fn xdnd_root_position(packed: u32) -> (i16, i16) {
    ((packed >> 16) as u16 as i16, packed as u16 as i16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpi::PhysicalPosition;

    #[test]
    fn logical_drag_points_apply_scale_once_and_reject_invalid_points() {
        for scale in [1.0, 1.25, 2.0] {
            assert_eq!(
                logical_position(40.5, 25.0, scale),
                Some(PhysicalPosition::new(40.5 * scale, 25.0 * scale))
            );
        }
        assert!(logical_position(f64::NAN, 1.0, 1.0).is_none());
        assert!(logical_position(1.0, 2.0, 0.0).is_none());
        assert!(logical_position(1.0, 2.0, f64::INFINITY).is_none());
        assert!(logical_position(f64::MAX, 1.0, 2.0).is_none());
    }

    #[test]
    fn screen_points_use_client_origin_without_rescaling_or_overflow() {
        assert_eq!(
            screen_position((-1800, 250), (-1920, 100)),
            PhysicalPosition::new(120.0, 150.0)
        );
        assert_eq!(
            screen_position((i32::MAX, i32::MIN), (i32::MIN, i32::MAX)),
            PhysicalPosition::new(4_294_967_295.0, -4_294_967_295.0)
        );
    }

    #[test]
    fn xdnd_coordinates_preserve_both_signed_monitor_axes() {
        for point in [(120_i16, 250_i16), (-1920, 120), (45, -1080), (i16::MIN, i16::MAX)] {
            let packed = (u32::from(point.0 as u16) << 16) | u32::from(point.1 as u16);
            assert_eq!(xdnd_root_position(packed), point);
        }
    }
}
