//! ディスプレイ(モニタ)構成の判定(issue #38「remember window position」)。
//!
//! window位置の復元時、保存先モニタが切断されている可能性がある。この
//! モジュールは`MonitorFromRect(..., MONITOR_DEFAULTTONULL)`で「矩形がどの
//! モニタとも交差しないか」を判定する。Win32 APIに触れてよいのは
//! `fyler-fsops`だけ(AGENTS.md 絶対ルール3周辺の境界)なので、fyler-appは
//! この関数を通してのみオフスクリーン判定を行う。

use fyler_core::window::PhysicalRect;

/// 矩形(物理px、仮想デスクトップ座標系)がどのモニタとも交差しないかどうかを判定する。
///
/// `true`を返したら、呼び出し側は保存位置を使わずサイズ・maximizedだけを
/// 復元すること(DESIGN.md「オフスクリーンガード」)。
pub fn is_offscreen(rect: PhysicalRect) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};

    let win_rect = RECT {
        left: rect.x,
        top: rect.y,
        right: rect.x.saturating_add(rect.width),
        bottom: rect.y.saturating_add(rect.height),
    };
    let monitor = unsafe { MonitorFromRect(&raw const win_rect, MONITOR_DEFAULTTONULL) };
    monitor.is_invalid()
}

// 実際のモニタ配置を伴う検証(複数モニタ・DPI混在)はWindows実機側の責務。
// ここではWindows専用APIをcfg(windows)クロスコンパイル配下でも保守しやすい
// 形にすることだけを目的とする最小テストを置く(CIのWindows GNUクロスは
// clippyのみでtestは実行しない)。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absurdly_large_rect_is_offscreen() {
        assert!(is_offscreen(PhysicalRect {
            x: i32::MAX / 2,
            y: i32::MAX / 2,
            width: 10,
            height: 10,
        }));
    }
}
