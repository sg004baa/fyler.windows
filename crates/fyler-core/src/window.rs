//! GUI toolkitに依存しないnative window geometry。

/// 正常終了時に保存し、次回起動時に復元するwindow geometry。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowGeometry {
    pub inner_width: f32,
    pub inner_height: f32,
    pub outer_x: f32,
    pub outer_y: f32,
    pub maximized: bool,
}

impl WindowGeometry {
    pub fn new(
        inner_width: f32,
        inner_height: f32,
        outer_x: f32,
        outer_y: f32,
        maximized: bool,
    ) -> Option<Self> {
        let geometry = Self {
            inner_width,
            inner_height,
            outer_x,
            outer_y,
            maximized,
        };
        geometry.is_valid().then_some(geometry)
    }

    pub fn is_valid(self) -> bool {
        self.inner_width.is_finite()
            && self.inner_width > 0.0
            && self.inner_height.is_finite()
            && self.inner_height > 0.0
            && self.outer_x.is_finite()
            && self.outer_y.is_finite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_geometry() {
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false).is_some());
        assert!(WindowGeometry::new(0.0, 600.0, 10.0, 20.0, false).is_none());
        assert!(WindowGeometry::new(800.0, f32::NAN, 10.0, 20.0, false).is_none());
        assert!(WindowGeometry::new(800.0, 600.0, f32::INFINITY, 20.0, false).is_none());
    }
}
