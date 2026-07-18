//! GUI toolkitに依存しないnative window geometry。

/// 正常終了時に保存し、次回起動時に復元するwindow geometry。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowGeometry {
    pub inner_width: f32,
    pub inner_height: f32,
    pub outer_x: f32,
    pub outer_y: f32,
    pub maximized: bool,
    /// 採取時の`ctx.pixels_per_point()`(論理pt→物理pxの換算係数)。
    ///
    /// eframe/winitはwindow生成時、`ViewportBuilder::with_position`の論理ptを
    /// プライマリモニタのscaleで物理pxへ変換するため、scaleの異なるセカンダリ
    /// モニタでは復元位置がズレ得る(winit issue #2645)。GUI層はこの値を使って
    /// 起動直後に実際の配置とのズレを検出し、`ViewportCommand::OuterPosition`で
    /// 一度だけ補正する。また保存先モニタが切断されたかどうかのオフスクリーン
    /// 判定(`fyler_fsops::display::is_offscreen`)にも使う(物理pxへの換算)。
    pub scale: f32,
}

impl WindowGeometry {
    pub fn new(
        inner_width: f32,
        inner_height: f32,
        outer_x: f32,
        outer_y: f32,
        maximized: bool,
        scale: f32,
    ) -> Option<Self> {
        let geometry = Self {
            inner_width,
            inner_height,
            outer_x,
            outer_y,
            maximized,
            scale,
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
            && self.scale.is_finite()
            && self.scale > 0.0
    }

    /// 保存位置・サイズを物理px(仮想デスクトップ座標系)の矩形へ換算する。
    ///
    /// オフスクリーン判定(`fyler_fsops::display::is_offscreen`)専用。装飾は
    /// 無効化されている(`with_decorations(false)`)ため、内側サイズを外枠の
    /// 近似として使う。
    pub fn physical_outer_rect(self) -> PhysicalRect {
        PhysicalRect {
            x: (self.outer_x * self.scale).round() as i32,
            y: (self.outer_y * self.scale).round() as i32,
            width: (self.inner_width * self.scale).round().max(1.0) as i32,
            height: (self.inner_height * self.scale).round().max(1.0) as i32,
        }
    }
}

/// window位置・サイズを物理px(仮想デスクトップ座標系)で表した矩形。
///
/// [`WindowGeometry::physical_outer_rect`] の出力型。`fyler-fsops`のオフスクリーン
/// 判定はWin32 APIに直接触れる唯一のクレートなので、この型はプリミティブだけで
/// 構成し、windowsクレートの型を露出させない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// GUI起動時に適用するwindow geometry。
///
/// `apply_position`が`false`の場合、`geometry.outer_x`/`outer_y`は無視して
/// サイズと`maximized`だけ適用する(保存先モニタが切断されている場合の
/// オフスクリーンガード。判定は`fyler_fsops::display::is_offscreen`、
/// Windows専用)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StartupWindow {
    pub geometry: WindowGeometry,
    pub apply_position: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_geometry() {
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false, 1.0).is_some());
        assert!(WindowGeometry::new(0.0, 600.0, 10.0, 20.0, false, 1.0).is_none());
        assert!(WindowGeometry::new(800.0, f32::NAN, 10.0, 20.0, false, 1.0).is_none());
        assert!(WindowGeometry::new(800.0, 600.0, f32::INFINITY, 20.0, false, 1.0).is_none());
    }

    #[test]
    fn rejects_invalid_scale() {
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false, 0.0).is_none());
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false, -1.0).is_none());
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false, f32::NAN).is_none());
        assert!(WindowGeometry::new(800.0, 600.0, 10.0, 20.0, false, 1.5).is_some());
    }

    #[test]
    fn physical_outer_rect_scales_logical_points_to_pixels() {
        let geometry = WindowGeometry::new(800.0, 600.0, 100.0, 50.0, false, 1.5).unwrap();
        assert_eq!(
            geometry.physical_outer_rect(),
            PhysicalRect {
                x: 150,
                y: 75,
                width: 1200,
                height: 900,
            }
        );
    }

    #[test]
    fn physical_outer_rect_at_unit_scale_matches_logical_values() {
        let geometry = WindowGeometry::new(800.0, 600.0, -20.0, 30.0, false, 1.0).unwrap();
        assert_eq!(
            geometry.physical_outer_rect(),
            PhysicalRect {
                x: -20,
                y: 30,
                width: 800,
                height: 600,
            }
        );
    }
}
