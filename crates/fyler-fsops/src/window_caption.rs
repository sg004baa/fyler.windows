//! App-painted caption controls integrated with native non-client input.
use std::{
    cell::RefCell,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context, anyhow, bail};
use fyler_core::window::{NativeCaptionLayout, NativeWindowCaption, PhysicalRect};
use windows::Win32::{
    Foundation::{E_INVALIDARG, E_NOTIMPL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMWA_CAPTION_BUTTON_BOUNDS, DWMWA_CAPTION_COLOR, DwmDefWindowProc,
            DwmExtendFrameIntoClientArea, DwmGetWindowAttribute, DwmSetWindowAttribute,
        },
        Gdi::{BLACK_BRUSH, FillRect, GetStockObject, HBRUSH, InvalidateRect, ScreenToClient},
    },
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{
        Controls::HOVER_DEFAULT,
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Input::KeyboardAndMouse::{
            GetKeyState, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent, VK_LBUTTON,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const SUBCLASS_ID: usize = 0x4659_4C45;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
impl BoxRect {
    fn from_physical(r: PhysicalRect) -> Self {
        Self {
            left: r.x,
            top: r.y,
            right: r.x.saturating_add(r.width),
            bottom: r.y.saturating_add(r.height),
        }
    }
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

fn client_caption_rect(caption: RECT, origin_client: (i32, i32)) -> PhysicalRect {
    let left = caption.left.saturating_add(origin_client.0);
    let top = caption.top.saturating_add(origin_client.1);
    let right = caption.right.saturating_add(origin_client.0);
    let bottom = caption.bottom.saturating_add(origin_client.1);
    PhysicalRect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

fn right_anchor_caption_rect(native: PhysicalRect, client_right: i32) -> PhysicalRect {
    PhysicalRect {
        x: client_right.saturating_sub(native.width),
        ..native
    }
}

#[cfg(test)]
mod right_anchor_tests {
    use super::*;

    #[test]
    fn right_anchor_preserves_dwm_width_and_flushes_to_client_edge() {
        let native = PhysicalRect {
            x: 692,
            y: 3,
            width: 100,
            height: 40,
        };
        let anchored = right_anchor_caption_rect(native, 800);
        assert_eq!(anchored, PhysicalRect { x: 700, ..native });
        let cells = custom_button_cells(anchored);
        assert_eq!(cells[1].x - cells[0].x, cells[2].x - cells[1].x);
        assert_eq!(cells[2].x + cells[2].width, 800);
        let close = BoxRect::from_physical(cells[2]);
        assert!(close.contains(799, 3));
        assert!(!close.contains(800, 3));
    }

    #[test]
    fn right_anchor_handles_native_overhang_and_narrow_client() {
        let native = PhysicalRect {
            x: 702,
            y: 0,
            width: 120,
            height: 34,
        };
        assert_eq!(right_anchor_caption_rect(native, 800).x, 680);
        assert_eq!(right_anchor_caption_rect(native, 50).x, -70);
    }
}

fn resize_hit(point: POINT, client: RECT, frame_x: i32, frame_y: i32) -> Option<isize> {
    let left = point.x < client.left.saturating_add(frame_x);
    let right = point.x >= client.right.saturating_sub(frame_x);
    let top = point.y < client.top.saturating_add(frame_y);
    let bottom = point.y >= client.bottom.saturating_sub(frame_y);
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(HTTOPLEFT as isize),
        (_, true, true, _) => Some(HTTOPRIGHT as isize),
        (true, _, _, true) => Some(HTBOTTOMLEFT as isize),
        (_, true, _, true) => Some(HTBOTTOMRIGHT as isize),
        (true, _, _, _) => Some(HTLEFT as isize),
        (_, true, _, _) => Some(HTRIGHT as isize),
        (_, _, true, _) => Some(HTTOP as isize),
        (_, _, _, true) => Some(HTBOTTOM as isize),
        _ => None,
    }
}

fn effective_toolbar_height(requested: u32, buttons: PhysicalRect) -> u32 {
    requested.max(buttons.y.saturating_add(buttons.height).max(0) as u32)
}

fn custom_button_cells(buttons: PhysicalRect) -> [PhysicalRect; 3] {
    let edge = |i: i32| {
        buttons
            .x
            .saturating_add(i.saturating_mul(buttons.width) / 3)
    };
    std::array::from_fn(|i| PhysicalRect {
        x: edge(i as i32),
        y: 0,
        width: edge(i as i32 + 1).saturating_sub(edge(i as i32)),
        height: buttons.height,
    })
}

fn custom_button_hit(
    buttons: PhysicalRect,
    x: i32,
    y: i32,
    style: u32,
    info: &TITLEBARINFOEX,
) -> Option<(isize, PhysicalRect)> {
    let hits = [HTMINBUTTON as isize, HTMAXBUTTON as isize, HTCLOSE as isize];
    let indices = [2, 3, 5];
    custom_button_cells(buttons)
        .into_iter()
        .enumerate()
        .find_map(|(i, cell)| {
            let enabled = match i {
                0 => style & WS_MINIMIZEBOX.0 != 0,
                1 => style & WS_MAXIMIZEBOX.0 != 0,
                _ => true,
            };
            let disabled =
                info.rgstate[indices[i]] & (0x0000_8000 | 0x0001_0000 | 0x0000_0001) != 0;
            (enabled && !disabled && BoxRect::from_physical(cell).contains(x, y))
                .then_some((hits[i], cell))
        })
}

fn titlebar_info(hwnd: HWND) -> TITLEBARINFOEX {
    let mut info = TITLEBARINFOEX {
        cbSize: std::mem::size_of::<TITLEBARINFOEX>() as u32,
        ..Default::default()
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_GETTITLEBARINFOEX,
            Some(WPARAM(0)),
            Some(LPARAM((&mut info as *mut TITLEBARINFOEX) as isize)),
        )
    };
    info
}
fn set_hovered_button(state: &Rc<RefCell<State>>, hovered: Option<PhysicalRect>) {
    let repaint = {
        let mut s = state.borrow_mut();
        let hovered = if s.dirty || s.layout.is_none() {
            None
        } else {
            hovered
        };
        if s.hovered_button == hovered {
            None
        } else {
            s.hovered_button = hovered;
            Some(s.repaint.clone())
        }
    };
    if let Some(repaint) = repaint {
        repaint();
    }
}

fn caption_color_ref(rgb: [u8; 3]) -> u32 {
    u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16)
}
fn set_caption_color(hwnd: HWND, rgb: [u8; 3]) -> windows::core::Result<bool> {
    let color = caption_color_ref(rgb);
    match unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            &color as *const u32 as *const c_void,
            std::mem::size_of_val(&color) as u32,
        )
    } {
        Ok(()) => Ok(true),
        Err(error) if error.code() == E_INVALIDARG || error.code() == E_NOTIMPL => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[test]
fn caption_background_rgb_converts_to_win32_colorref() {
    assert_eq!(caption_color_ref([17, 34, 51]), 0x0033_2211);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_button_hit_uses_contiguous_client_geometry_and_metadata_availability() {
        let buttons = PhysicalRect {
            x: -1801,
            y: 0,
            width: 173,
            height: 43,
        };
        let mut info = TITLEBARINFOEX::default();
        info.rgrect[2] = RECT {
            left: 9000,
            top: 9000,
            right: 9100,
            bottom: 9100,
        };
        info.rgrect[3] = RECT {
            left: 9200,
            top: 9200,
            right: 9300,
            bottom: 9300,
        };
        info.rgrect[5] = RECT {
            left: 9400,
            top: 9400,
            right: 9500,
            bottom: 9500,
        };
        let cells = [
            PhysicalRect {
                x: -1801,
                y: 0,
                width: 57,
                height: 43,
            },
            PhysicalRect {
                x: -1744,
                y: 0,
                width: 58,
                height: 43,
            },
            PhysicalRect {
                x: -1686,
                y: 0,
                width: 58,
                height: 43,
            },
        ];
        let hits = [HTMINBUTTON as isize, HTMAXBUTTON as isize, HTCLOSE as isize];
        let style = WS_MINIMIZEBOX.0 | WS_MAXIMIZEBOX.0;
        for (cell, hit) in cells.into_iter().zip(hits) {
            for y in [0, 42] {
                assert_eq!(
                    custom_button_hit(buttons, cell.x, y, style, &info),
                    Some((hit, cell))
                );
            }
        }
        for (x, y) in [(-1802, 0), (-1801, -1), (-1801, 43), (-1628, 0)] {
            assert_eq!(custom_button_hit(buttons, x, y, style, &info), None);
        }
        for state in [0x0000_8000, 0x0001_0000, 0x0000_0001] {
            let mut unavailable = TITLEBARINFOEX::default();
            unavailable.rgstate[2] = state;
            assert_eq!(
                custom_button_hit(buttons, -1800, 0, style, &unavailable),
                None
            );
        }
        assert_eq!(
            custom_button_hit(buttons, -1800, 0, WS_MAXIMIZEBOX.0, &info),
            None
        );
        assert_eq!(
            custom_button_hit(buttons, -1743, 0, WS_MINIMIZEBOX.0, &info),
            None
        );
    }

    #[test]
    fn converts_window_relative_caption_bounds_to_client_coordinates_on_moved_negative_monitor() {
        let bounds = client_caption_rect(
            RECT {
                left: 1300,
                top: 40,
                right: 1400,
                bottom: 80,
            },
            (-1250, 72),
        );
        assert_eq!(
            bounds,
            PhysicalRect {
                x: 50,
                y: 112,
                width: 100,
                height: 40
            }
        );
    }

    #[test]
    fn converts_caption_bounds_with_nonzero_maximized_client_inset() {
        let bounds = client_caption_rect(
            RECT {
                left: 900,
                top: 20,
                right: 1000,
                bottom: 60,
            },
            (-900, 32),
        );
        assert_eq!(
            bounds,
            PhysicalRect {
                x: 0,
                y: 52,
                width: 100,
                height: 40
            }
        );
    }

    #[test]
    fn resize_hit_uses_client_local_point_and_bounds() {
        let client = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        assert_eq!(
            resize_hit(POINT { x: 2, y: 2 }, client, 8, 10),
            Some(HTTOPLEFT as isize)
        );
        assert_eq!(
            resize_hit(POINT { x: 400, y: 4 }, client, 8, 10),
            Some(HTTOP as isize)
        );
        assert_eq!(resize_hit(POINT { x: 400, y: 300 }, client, 8, 10), None);
    }

    #[test]
    fn toolbar_reservation_covers_requested_height_and_caption_buttons() {
        assert_eq!(
            effective_toolbar_height(
                32,
                PhysicalRect {
                    x: 900,
                    y: 0,
                    width: 100,
                    height: 44
                }
            ),
            44
        );
        assert_eq!(
            effective_toolbar_height(
                48,
                PhysicalRect {
                    x: 900,
                    y: 0,
                    width: 100,
                    height: 44
                }
            ),
            48
        );
        assert_eq!(
            effective_toolbar_height(
                32,
                PhysicalRect {
                    x: 900,
                    y: 20,
                    width: 100,
                    height: 44
                }
            ),
            64
        );
    }
}

struct State {
    repaint: Arc<dyn Fn() + Send + Sync>,
    background_rgb: [u8; 3],
    native_color_enabled: bool,
    dirty: bool,
    extend_dirty: bool,
    height: u32,
    extended_height: Option<u32>,
    layout: Option<NativeCaptionLayout>,
    hovered_button: Option<PhysicalRect>,
    drag: Option<BoxRect>,
    failure: Option<String>,
    alive: bool,
    revision: u64,
}
pub struct NativeCaptionHost {
    hwnd: HWND,
    state: Rc<RefCell<State>>,
    installed: bool,
}

pub fn install(
    hwnd: isize,
    repaint: Arc<dyn Fn() + Send + Sync>,
    background_rgb: [u8; 3],
) -> anyhow::Result<Box<dyn NativeWindowCaption>> {
    let hwnd = HWND(hwnd as *mut c_void);
    if hwnd.0.is_null() || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        bail!("invalid native window handle")
    }
    let mut pid = 0;
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if tid == 0 || pid != unsafe { GetCurrentProcessId() } {
        bail!("native window must belong to the current process and GUI thread")
    }
    if unsafe { GetCurrentThreadId() } != tid {
        bail!("native caption must be installed on the window's owning thread")
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    if style & (WS_CAPTION.0 | WS_SYSMENU.0) != (WS_CAPTION.0 | WS_SYSMENU.0) {
        bail!("window lacks required system caption styles")
    }
    let state = Rc::new(RefCell::new(State {
        repaint,
        background_rgb,
        native_color_enabled: true,
        dirty: true,
        extend_dirty: true,
        height: 1,
        extended_height: None,
        layout: None,
        hovered_button: None,
        drag: None,
        failure: None,
        alive: true,
        revision: 0,
    }));
    let data = Rc::into_raw(state.clone()) as usize;
    if !unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, data) }.as_bool() {
        unsafe { drop(Rc::from_raw(data as *const RefCell<State>)) }
        bail!("SetWindowSubclass failed")
    }
    let host = Box::new(NativeCaptionHost {
        hwnd,
        state,
        installed: true,
    });
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    }
    .context("SetWindowPos(SWP_FRAMECHANGED) failed during native caption installation")?;
    let color_supported = set_caption_color(hwnd, background_rgb)
        .context("DwmSetWindowAttribute(DWMWA_CAPTION_COLOR) failed")?;
    if !color_supported {
        host.state.borrow_mut().native_color_enabled = false;
    }
    unsafe { InvalidateRect(Some(hwnd), None, true) }
        .ok()
        .context("InvalidateRect failed during native caption installation")?;
    Ok(host)
}

impl NativeWindowCaption for NativeCaptionHost {
    fn update(&mut self, toolbar_height: u32) -> anyhow::Result<Option<NativeCaptionLayout>> {
        let requested = toolbar_height.max(1);
        {
            let mut s = self.state.borrow_mut();
            if !s.alive {
                bail!("native window was destroyed")
            }
            if let Some(e) = s.failure.take() {
                return Err(anyhow!("native caption callback failed: {e}"));
            }
            if s.height != requested {
                s.height = requested;
                s.dirty = true;
                s.extend_dirty = true;
                s.revision = s.revision.wrapping_add(1)
            }
        }
        let visible = unsafe { IsWindowVisible(self.hwnd) }.as_bool()
            && !unsafe { IsIconic(self.hwnd) }.as_bool();
        if !visible {
            let repaint = {
                let mut s = self.state.borrow_mut();
                let had_layout = s.layout.take().is_some();
                let had_hover = s.hovered_button.take().is_some();
                let changed = had_layout || had_hover;
                s.dirty = true;
                s.extend_dirty = true;
                s.revision = s.revision.wrapping_add(1);
                changed.then(|| s.repaint.clone())
            };
            if let Some(repaint) = repaint {
                repaint();
            }
            return Ok(None);
        }

        let (revision, dirty, extend_dirty, requested, old_layout, already_extended) = {
            let s = self.state.borrow();
            (
                s.revision,
                s.dirty,
                s.extend_dirty,
                s.height,
                s.layout,
                s.extended_height,
            )
        };
        if !dirty && !extend_dirty {
            return Ok(old_layout.map(|mut layout| {
                layout.hovered_button = self.state.borrow().hovered_button;
                layout
            }));
        }
        let layout = if dirty || old_layout.is_none() {
            let mut wr = RECT::default();
            unsafe { GetWindowRect(self.hwnd, &mut wr) }.context("GetWindowRect failed")?;
            let mut cap = RECT::default();
            unsafe {
                DwmGetWindowAttribute(
                    self.hwnd,
                    DWMWA_CAPTION_BUTTON_BOUNDS,
                    &mut cap as *mut _ as *mut _,
                    std::mem::size_of::<RECT>() as u32,
                )
            }
            .context("DwmGetWindowAttribute(DWMWA_CAPTION_BUTTON_BOUNDS) failed")?;
            let mut origin = POINT {
                x: wr.left,
                y: wr.top,
            };
            if !unsafe { ScreenToClient(self.hwnd, &mut origin) }.as_bool() {
                bail!("ScreenToClient failed")
            }
            let mut client = RECT::default();
            unsafe { GetClientRect(self.hwnd, &mut client) }
                .context("GetClientRect failed during caption geometry update")?;
            let native_buttons = right_anchor_caption_rect(
                client_caption_rect(cap, (origin.x, origin.y)),
                client.right,
            );
            if native_buttons.width <= 0 || native_buttons.height <= 0 {
                bail!("DWM returned invalid caption button bounds")
            }
            let toolbar_height = effective_toolbar_height(requested, native_buttons);
            let buttons = PhysicalRect {
                x: native_buttons.x,
                y: 0,
                width: native_buttons.width,
                height: i32::try_from(toolbar_height)
                    .context("toolbar height exceeds Win32 limits")?,
            };
            Some(NativeCaptionLayout {
                buttons,
                hovered_button: None,
                toolbar_height,
            })
        } else {
            old_layout
        };
        let mut layout = layout.expect("dirty caption update computes layout");
        let target_height = layout.toolbar_height;
        if extend_dirty || already_extended != Some(target_height) {
            let h = i32::try_from(target_height).context("toolbar height exceeds Win32 limits")?;
            let margins = windows::Win32::UI::Controls::MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: h,
                cyBottomHeight: 0,
            };
            unsafe { DwmExtendFrameIntoClientArea(self.hwnd, &margins) }
                .context("DwmExtendFrameIntoClientArea failed")?;
            unsafe { InvalidateRect(Some(self.hwnd), None, true) }
                .ok()
                .context("InvalidateRect failed after extending the DWM frame")?;
        }
        let mut s = self.state.borrow_mut();
        layout.hovered_button = s.hovered_button;
        if s.revision == revision {
            s.layout = Some(layout);
            s.dirty = false;
            s.extended_height = Some(target_height);
            s.extend_dirty = false;
        }
        Ok(Some(layout))
    }

    fn set_drag_region(&mut self, region: Option<PhysicalRect>) {
        self.state.borrow_mut().drag = region.map(BoxRect::from_physical)
    }
}

impl Drop for NativeCaptionHost {
    fn drop(&mut self) {
        if !self.installed {
            return;
        }
        let alive = self.state.borrow().alive;
        if alive
            && unsafe { RemoveWindowSubclass(self.hwnd, Some(subclass_proc), SUBCLASS_ID) }
                .as_bool()
        {
            unsafe { drop(Rc::from_raw(Rc::as_ptr(&self.state))) }
        }
        self.installed = false;
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        subclass_proc_inner(hwnd, msg, w, l, id, data)
    }))
    .unwrap_or_else(|_| unsafe { DefSubclassProc(hwnd, msg, w, l) })
}

unsafe fn subclass_proc_inner(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let ptr = data as *const RefCell<State>;
    if ptr.is_null() {
        return unsafe { DefSubclassProc(hwnd, msg, w, l) };
    }
    let raw_owner = unsafe { Rc::from_raw(ptr) };
    let state = raw_owner.clone();
    let _ = Rc::into_raw(raw_owner);

    let mut dwm = LRESULT(0);
    let dwm_handled = unsafe { DwmDefWindowProc(hwnd, msg, w, l, &mut dwm) }.as_bool();
    if msg == WM_NCDESTROY {
        let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), id) };
        {
            let mut s = state.borrow_mut();
            s.alive = false;
            s.dirty = true;
            s.revision = s.revision.wrapping_add(1);
        }
        let result = unsafe { DefSubclassProc(hwnd, msg, w, l) };
        unsafe { drop(Rc::from_raw(ptr)) };
        return result;
    }
    if matches!(msg, WM_NCLBUTTONDOWN | WM_NCMOUSELEAVE) {
        set_hovered_button(&state, None);
    }
    if msg == WM_NCMOUSEMOVE {
        let mut point = POINT {
            x: (l.0 as u32 as u16 as i16) as i32,
            y: ((l.0 as u32 >> 16) as u16 as i16) as i32,
        };
        let layout = state.borrow().layout;
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        let hovered = if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() {
            record_failure(&state, "ScreenToClient failed during caption hover");
            None
        } else {
            layout.and_then(|layout| {
                custom_button_hit(
                    layout.buttons,
                    point.x,
                    point.y,
                    style,
                    &titlebar_info(hwnd),
                )
                .map(|(_, cell)| cell)
            })
        };
        let button_down = unsafe { GetKeyState(VK_LBUTTON.0 as i32) } < 0;
        set_hovered_button(&state, if button_down { None } else { hovered });
        let mut tracking = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_NONCLIENT | TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: HOVER_DEFAULT,
        };
        if let Err(error) = unsafe { TrackMouseEvent(&mut tracking) } {
            record_failure(&state, &format!("TrackMouseEvent failed: {error}"));
        }
    }
    if msg == WM_NCACTIVATE || msg == WM_ACTIVATE {
        invalidate(&state, true);
        let (color_enabled, rgb) = {
            let s = state.borrow();
            (s.native_color_enabled, s.background_rgb)
        };
        if color_enabled {
            match set_caption_color(hwnd, rgb) {
                Ok(true) => {}
                Ok(false) => state.borrow_mut().native_color_enabled = false,
                Err(error) => record_failure(
                    &state,
                    &format!("DwmSetWindowAttribute(DWMWA_CAPTION_COLOR) failed: {error}"),
                ),
            }
        }
        return unsafe { DefSubclassProc(hwnd, msg, w, l) };
    }
    if dwm_handled
        && msg != WM_NCHITTEST
        && matches!(
            msg,
            WM_NCLBUTTONDOWN
                | WM_NCLBUTTONUP
                | WM_NCLBUTTONDBLCLK
                | WM_NCRBUTTONDOWN
                | WM_NCRBUTTONUP
                | WM_NCRBUTTONDBLCLK
                | WM_NCPAINT
        )
    {
        return dwm;
    }
    if msg == WM_NCHITTEST {
        let screen_x = (l.0 as u32 as u16 as i16) as i32;
        let screen_y = ((l.0 as u32 >> 16) as u16 as i16) as i32;
        let mut point = POINT {
            x: screen_x,
            y: screen_y,
        };
        if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() {
            record_failure(&state, "ScreenToClient failed during hit testing");
            return unsafe { DefSubclassProc(hwnd, msg, w, l) };
        }
        let layout = state.borrow().layout;
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        let info = titlebar_info(hwnd);
        if let Some((hit, _)) = layout
            .and_then(|layout| custom_button_hit(layout.buttons, point.x, point.y, style, &info))
        {
            return LRESULT(hit);
        }
        if dwm_handled
            && !matches!(dwm.0, x if x == HTMINBUTTON as isize || x == HTMAXBUTTON as isize || x == HTCLOSE as isize)
        {
            return dwm;
        }
    }
    if msg == WM_ERASEBKGND {
        let mut client = RECT::default();
        if let Err(error) = unsafe { GetClientRect(hwnd, &mut client) } {
            record_failure(
                &state,
                &format!("GetClientRect failed during background erase: {error}"),
            );
            return unsafe { DefSubclassProc(hwnd, msg, w, l) };
        }
        let brush = unsafe { GetStockObject(BLACK_BRUSH) };
        let result = unsafe {
            FillRect(
                windows::Win32::Graphics::Gdi::HDC(w.0 as *mut c_void),
                &client,
                HBRUSH(brush.0),
            )
        };
        if result == 0 {
            record_failure(&state, "FillRect failed during background erase");
            return unsafe { DefSubclassProc(hwnd, msg, w, l) };
        }
        return LRESULT(1);
    }
    match msg {
        WM_SHOWWINDOW
        | WM_ACTIVATE
        | WM_SIZE
        | WM_DPICHANGED
        | WM_SETTINGCHANGE
        | WM_THEMECHANGED
        | WM_DWMCOMPOSITIONCHANGED
        | WM_NCACTIVATE => {
            let extend = matches!(msg, WM_ACTIVATE | WM_NCACTIVATE | WM_DWMCOMPOSITIONCHANGED);
            invalidate(&state, extend);
            if let Err(error) = unsafe { InvalidateRect(Some(hwnd), None, true) }.ok() {
                record_failure(&state, &format!("InvalidateRect failed: {error}"));
            }
            let cb = { state.borrow().repaint.clone() };
            cb();
        }
        WM_NCHITTEST => {
            let mut p = POINT {
                x: (l.0 as u32 as u16 as i16) as i32,
                y: ((l.0 as u32 >> 16) as u16 as i16) as i32,
            };
            if !unsafe { ScreenToClient(hwnd, &mut p) }.as_bool() {
                record_failure(&state, "ScreenToClient failed during hit testing");
                return unsafe { DefSubclassProc(hwnd, msg, w, l) };
            }
            let mut client = RECT::default();
            if let Err(e) = unsafe { GetClientRect(hwnd, &mut client) } {
                record_failure(
                    &state,
                    &format!("GetClientRect failed during hit testing: {e}"),
                );
                return unsafe { DefSubclassProc(hwnd, msg, w, l) };
            }
            let (buttons, drag) = {
                let s = state.borrow();
                (s.layout.map(|v| BoxRect::from_physical(v.buttons)), s.drag)
            };
            if buttons.is_some_and(|r| r.contains(p.x, p.y)) {
                // The app owns this geometry; native defaults must not resurrect
                // caption buttons that are unavailable in the custom hit test.
                return LRESULT(HTCLIENT as isize);
            }
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
            let can_resize = style & WS_CAPTION.0 != 0
                && style & WS_THICKFRAME.0 != 0
                && !unsafe { IsZoomed(hwnd) }.as_bool();
            if can_resize {
                let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
                let fx = (unsafe {
                    GetSystemMetricsForDpi(SM_CXSIZEFRAME, dpi)
                        + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                })
                .max(1);
                let fy = (unsafe {
                    GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                        + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                })
                .max(1);
                if let Some(hit) = resize_hit(p, client, fx, fy) {
                    return LRESULT(hit);
                }
            }
            if drag.is_some_and(|r| r.contains(p.x, p.y)) {
                return LRESULT(HTCAPTION as isize);
            }
        }
        _ => {}
    }
    unsafe { DefSubclassProc(hwnd, msg, w, l) }
}

fn record_failure(state: &Rc<RefCell<State>>, message: &str) {
    let repaint = {
        let mut s = state.borrow_mut();
        s.failure = Some(message.to_owned());
        s.repaint.clone()
    };
    repaint();
}

fn invalidate(state: &Rc<RefCell<State>>, extend: bool) {
    let mut s = state.borrow_mut();
    s.dirty = true;
    s.extend_dirty |= extend;
    s.hovered_button = None;
    if let Some(layout) = &mut s.layout {
        layout.hovered_button = None;
    }
    s.revision = s.revision.wrapping_add(1);
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, w, l) }
    }

    #[test]
    fn hidden_window_install_update_drop_reinstall_destroy_update_errors_safely() {
        let class: [u16; 17] = [
            b'F' as u16,
            b'y' as u16,
            b'l' as u16,
            b'e' as u16,
            b'r' as u16,
            b'C' as u16,
            b'a' as u16,
            b'p' as u16,
            b't' as u16,
            b'i' as u16,
            b'o' as u16,
            b'n' as u16,
            b'T' as u16,
            b'e' as u16,
            b's' as u16,
            b't' as u16,
            0,
        ];
        let name = windows::core::PCWSTR(class.as_ptr());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: name,
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&wc) }, 0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                name,
                windows::core::PCWSTR(class.as_ptr()),
                WS_CAPTION | WS_SYSMENU | WS_SIZEBOX,
                0,
                0,
                200,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let mut first = install(hwnd.0 as isize, repaint.clone(), [17, 17, 20]).unwrap();
        assert_eq!(first.update(32).unwrap(), None);
        drop(first);
        let mut second = install(hwnd.0 as isize, repaint, [17, 17, 20]).unwrap();
        let mut color = 0u32;
        let initial_color_result = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CAPTION_COLOR,
                &mut color as *mut u32 as *mut c_void,
                std::mem::size_of_val(&color) as u32,
            )
        };
        match initial_color_result {
            Ok(()) => {
                assert_eq!(color, caption_color_ref([17, 17, 20]));
                unsafe {
                    SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
                    SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
                }
                unsafe {
                    DwmGetWindowAttribute(
                        hwnd,
                        DWMWA_CAPTION_COLOR,
                        &mut color as *mut u32 as *mut c_void,
                        std::mem::size_of_val(&color) as u32,
                    )
                }
                .unwrap();
                assert_eq!(color, caption_color_ref([17, 17, 20]));
            }
            Err(error) if error.code() == E_INVALIDARG || error.code() == E_NOTIMPL => {
                unsafe {
                    SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
                    SendMessageW(hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
                }
                assert_eq!(second.update(32).unwrap(), None);
            }
            Err(error) => panic!("DwmGetWindowAttribute(DWMWA_CAPTION_COLOR) failed: {error}"),
        }
        unsafe { DestroyWindow(hwnd) }.unwrap();
        assert!(second.update(32).is_err());
        drop(second);
        unsafe { UnregisterClassW(name, None) }.unwrap();
    }

    #[test]
    fn erase_background_clears_white_dib_pixels_including_alpha() {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, SelectObject,
        };

        let class: Vec<u16> = "FylerCaptionEraseTest\0".encode_utf16().collect();
        let name = windows::core::PCWSTR(class.as_ptr());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: name,
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&wc) }, 0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                name,
                name,
                WS_CAPTION | WS_SYSMENU,
                0,
                0,
                200,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let dc = unsafe { CreateCompatibleDC(None) };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 200,
                biHeight: -100,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap =
            unsafe { CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) }
                .unwrap();
        {
            let pixels =
                unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), 200 * 100 * 4) };
            pixels.fill(0xff);
        }
        let old_bitmap = unsafe { SelectObject(dc, bitmap.into()) };
        let _host = install(hwnd.0 as isize, Arc::new(|| {}), [17, 17, 20]).unwrap();
        let result = unsafe {
            SendMessageW(
                hwnd,
                WM_ERASEBKGND,
                Some(WPARAM(dc.0 as usize)),
                Some(LPARAM(0)),
            )
        };
        assert_eq!(result.0, 1);
        unsafe { GdiFlush() }.ok().unwrap();
        let pixels = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), 200 * 100 * 4) };
        assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
        unsafe {
            SelectObject(dc, old_bitmap);
            DestroyWindow(hwnd).unwrap();
            DeleteObject(bitmap.into()).unwrap();
            DeleteDC(dc).unwrap();
            UnregisterClassW(name, None).unwrap();
        }
    }
}
