//! OLE drag source(fylerの選択entryをExplorer等の外部Shellターゲットへ
//! drag-and-dropで渡す)。
//!
//! fylerは`IDataObject`(CF_HDROP + "Preferred DropEffect")と`IDropSource`を
//! 公開するだけで、**実FSへの変更は一切行わない**(copy/moveの実行はdrop先の
//! アプリケーション)。Cut相当の後始末(source側のごみ箱退避)はapp層が
//! [`fyler_core::transfer::DragOutcome`]を見て確認ダイアログ承認後に行う
//! (絶対ルール1)。
//!
//! **意図的な例外**: drag payloadへ渡すパスは素の絶対パス(`\\?\` を付けない)。
//! Explorerとの相互運用のための意図的な例外であり、`crates/fyler-fsops/src/
//! clipboard.rs` / `terminal.rs` と同じ扱い(絶対ルール3: `\\?\` は
//! long_pathモジュールの1か所だけ)。
//!
//! # スレッド要件とWindows実機リスク
//! [`perform_drag`]は呼び出しスレッドで`OleInitialize`/`OleUninitialize`を
//! RAIIで行い、`DoDragDrop`で**dragが終わるまでblockする**。GUI(eframe)
//! スレッドからは呼ばず、app層が使い捨てSTAスレッドを起こして呼ぶ設計。
//! `DoDragDrop`はSTA + message pumpを要求するが、その要件は`DoDragDrop`自身の
//! モーダルループが満たす。ただしdragを開始したGUI window(別スレッド所有)と
//! mouse captureの関係はOLEの想定構成(UIスレッドからの呼び出し)と異なるため、
//! **capture遷移の振舞い(drag中のcursor形状・release検知)はWindows実機での
//! 検証が必要**。既知のリスク: GUI側がpointer captureを保持したままだと
//! `QueryContinueDrag`へ渡るbutton状態が古くなる可能性がある。
//!
//! # レイヤー構成(clipboard.rsと同じ3層)
//! - 純粋関数層(このモジュール冒頭): drag継続判定・FORMATETC適合判定・
//!   DoDragDrop結果→[`DragOutcome`]の畳み込み。cfg非依存でLinux上でも
//!   unit testできる。
//! - `cfg(windows)`層: `#[implement]`によるIDataObject/IDropSource実装と
//!   `DoDragDrop`呼び出し。
//! - `cfg(not(windows))`層: 明示的な`Err`を返す(silent fallback禁止)。

use std::path::PathBuf;

#[cfg(not(windows))]
use anyhow::bail;
use fyler_core::transfer::{DragOutcome, DropEffect};

/// DROPEFFECT bit値(OleIdl.h)。Shell共通の慣例値で全Windows版固定。
/// 純粋層をcfg非依存に保つためローカル定義する(clipboard.rsのencode値と同じ)。
const DROPEFFECT_COPY_BIT: u32 = 1;
const DROPEFFECT_MOVE_BIT: u32 = 2;
const DROPEFFECT_LINK_BIT: u32 = 4;

/// `TYMED_HGLOBAL` / `DVASPECT_CONTENT` のbit値(ObjIdl.h)。同じく固定値。
const TYMED_HGLOBAL_BIT: u32 = 1;
const DVASPECT_CONTENT_BIT: u32 = 1;

/// targetが`IDataObject::SetData`で書き込む「実際に行われた効果」の形式名。
#[cfg(windows)]
const PERFORMED_DROPEFFECT_FORMAT: &str = "Performed DropEffect";

// ---------------------------------------------------------------------
// 純粋関数層(cfg非依存)
// ---------------------------------------------------------------------

/// `IDropSource::QueryContinueDrag`の判定結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragContinue {
    /// dragを継続する(S_OK)。
    Continue,
    /// 左buttonが離された。dropを確定する(DRAGDROP_S_DROP)。
    Drop,
    /// Escが押された。dragをキャンセルする(DRAGDROP_S_CANCEL)。
    Cancel,
}

/// `QueryContinueDrag`の純ロジック。Escキャンセルを最優先し、
/// 左button解放でdrop確定、それ以外は継続。
fn continue_drag_decision(escape_pressed: bool, left_button_down: bool) -> DragContinue {
    if escape_pressed {
        DragContinue::Cancel
    } else if !left_button_down {
        DragContinue::Drop
    } else {
        DragContinue::Continue
    }
}

/// FORMATETCが「HGLOBAL渡しのcontent形式`candidate`」として提供可能かの純ロジック。
/// `tymed`と`aspect`はbit集合(呼び出し側は複数bitを立てて問い合わせてよい)。
fn format_supported(cf_format: u16, tymed: u32, aspect: u32, candidate: u16) -> bool {
    cf_format == candidate
        && tymed & TYMED_HGLOBAL_BIT != 0
        && aspect & DVASPECT_CONTENT_BIT != 0
}

/// `DoDragDrop`の完了状態(drop成立か)と、戻り値effect・targetが書き込んだ
/// "Performed DropEffect"から[`DragOutcome`]を判定する純ロジック。
///
/// moveの報告はどちらか一方にしか現れないことがある(Explorerは最適化moveで
/// 戻り値をNONEにし、"Performed DropEffect"側で報告する場合がある)ため、
/// 両方のMOVE bitをORで見る。move未報告の効果(copy/link)はsource側の
/// 後始末が不要なので[`DropEffect::Copy`]へ畳む。
fn resolve_outcome(dropped: bool, returned_effect: u32, performed_effect: Option<u32>) -> DragOutcome {
    if !dropped {
        return DragOutcome::Cancelled;
    }
    let move_reported = returned_effect & DROPEFFECT_MOVE_BIT != 0
        || performed_effect.is_some_and(|effect| effect & DROPEFFECT_MOVE_BIT != 0);
    DragOutcome::Dropped {
        effect: if move_reported {
            DropEffect::Move
        } else {
            DropEffect::Copy
        },
        move_reported,
    }
}

// ---------------------------------------------------------------------
// プラットフォーム層
// ---------------------------------------------------------------------

/// 絶対パス列をOLE drag sourceとして公開し、dragが終わるまでblockする。
///
/// 実FSは一切変更しない(copy/moveの実行はdrop先)。呼び出しスレッドで
/// `OleInitialize`/`OleUninitialize`をRAIIで行うため、**COM/OLE状態を持たない
/// 使い捨てスレッドから呼ぶこと**(モジュールdoc参照)。
#[cfg(windows)]
pub fn perform_drag(paths: &[PathBuf]) -> anyhow::Result<DragOutcome> {
    if paths.is_empty() {
        anyhow::bail!("No items to drag");
    }
    win::do_drag_drop(paths)
}

#[cfg(windows)]
mod win {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::anyhow;
    use fyler_core::transfer::DragOutcome;
    use windows::Win32::Foundation::{
        DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
        DV_E_FORMATETC, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, OLE_E_ADVISENOTSUPPORTED, S_OK,
    };
    use windows::Win32::System::Com::{
        DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
        IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Ole::{
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DoDragDrop, IDropSource,
        IDropSource_Impl, OleInitialize, OleUninitialize, ReleaseStgMedium,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc;
    use windows::core::{BOOL, HRESULT, implement};

    use crate::clipboard::win as clip_win;

    /// 呼び出しスレッドのOLEをRAIIで初期化・解放する。
    /// `OleInitialize`はSTAを要求する(MTA初期化済みスレッドではエラー)。
    struct OleApartment;

    impl OleApartment {
        fn initialize() -> anyhow::Result<Self> {
            // SAFETY: 現在スレッドに対するOLE初期化。成功(S_FALSE=再初期化を含む)
            // したら参照カウントを合わせるためDropで必ずOleUninitializeする。
            unsafe { OleInitialize(None) }
                .map_err(|error| anyhow!("Failed to initialize OLE for drag-and-drop: {error}"))?;
            Ok(Self)
        }
    }

    impl Drop for OleApartment {
        fn drop(&mut self) {
            // SAFETY: `initialize`で成功したOLE初期化と対応する終了処理。
            unsafe { OleUninitialize() };
        }
    }

    /// HGLOBAL渡しのcontent形式FORMATETCを作る。
    fn hglobal_format(cf_format: u16) -> FORMATETC {
        FORMATETC {
            cfFormat: cf_format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        }
    }

    /// drag sourceが公開するデータ(CF_HDROP + "Preferred DropEffect")。
    /// targetが`SetData`で書き込む"Performed DropEffect"はMutexへ記録し、
    /// `DoDragDrop`完了後に`do_drag_drop`が回収する。
    #[implement(IDataObject)]
    struct DragDataObject {
        hdrop_payload: Vec<u8>,
        preferred_payload: Vec<u8>,
        hdrop_format: u16,
        preferred_format: u16,
        performed_format: u16,
        performed_effect: Arc<Mutex<Option<u32>>>,
    }

    impl IDataObject_Impl for DragDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            // SAFETY: OLEは有効なFORMATETCポインタを渡す契約。nullは形式不一致扱い。
            let format = unsafe { pformatetcin.as_ref() }
                .ok_or_else(|| windows::core::Error::from_hresult(DV_E_FORMATETC))?;
            let payload = if super::format_supported(
                format.cfFormat,
                format.tymed,
                format.dwAspect,
                self.hdrop_format,
            ) {
                &self.hdrop_payload
            } else if super::format_supported(
                format.cfFormat,
                format.tymed,
                format.dwAspect,
                self.preferred_format,
            ) {
                &self.preferred_payload
            } else {
                return Err(windows::core::Error::from_hresult(DV_E_FORMATETC));
            };
            let handle = clip_win::alloc_hglobal_bytes(payload)
                .map_err(|_| windows::core::Error::from_hresult(E_OUTOFMEMORY))?;
            // 所有権は呼び出し元(target)へ移り、targetがReleaseStgMediumで解放する。
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: handle },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            // SAFETY: OLEは有効なFORMATETCポインタを渡す契約。nullは形式不一致扱い。
            let Some(format) = (unsafe { pformatetc.as_ref() }) else {
                return DV_E_FORMATETC;
            };
            let supported = super::format_supported(
                format.cfFormat,
                format.tymed,
                format.dwAspect,
                self.hdrop_format,
            ) || super::format_supported(
                format.cfFormat,
                format.tymed,
                format.dwAspect,
                self.preferred_format,
            );
            if supported { S_OK } else { DV_E_FORMATETC }
        }

        fn GetCanonicalFormatEtc(
            &self,
            pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if pformatetcout.is_null() {
                return E_POINTER;
            }
            // SAFETY: OLEは有効な入出力ポインタを渡す契約(nullは上で拒否)。
            unsafe {
                let Some(input) = pformatectin.as_ref() else {
                    return E_POINTER;
                };
                *pformatetcout = *input;
                (*pformatetcout).ptd = std::ptr::null_mut();
            }
            // device非依存のデータのみ提供するため、常に「同じ形式」で応答する。
            DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> windows::core::Result<()> {
            let result: windows::core::Result<()> = (|| {
                // SAFETY: OLEは有効なポインタを渡す契約。nullは形式不一致扱い。
                let format = unsafe { pformatetc.as_ref() }
                    .ok_or_else(|| windows::core::Error::from_hresult(DV_E_FORMATETC))?;
                let medium = unsafe { pmedium.as_ref() }
                    .ok_or_else(|| windows::core::Error::from_hresult(DV_E_FORMATETC))?;
                if format.cfFormat != self.performed_format
                    || medium.tymed != TYMED_HGLOBAL.0 as u32
                {
                    return Err(windows::core::Error::from_hresult(DV_E_FORMATETC));
                }
                // SAFETY: tymed=HGLOBALを確認済みのunion field。targetが確保した
                // 有効なhandleで、所有権はここでは移らない(コピーのみ)。
                let bytes = unsafe { clip_win::read_hglobal_bytes(medium.u.hGlobal) }
                    .map_err(|_| windows::core::Error::from_hresult(DV_E_FORMATETC))?;
                let raw: [u8; 4] = bytes
                    .get(..4)
                    .and_then(|head| head.try_into().ok())
                    .ok_or_else(|| windows::core::Error::from_hresult(DV_E_FORMATETC))?;
                *self
                    .performed_effect
                    .lock()
                    .expect("performed effect lock poisoned") = Some(u32::from_le_bytes(raw));
                Ok(())
            })();
            // fRelease=TRUEで成功した場合のみ所有権が移る(MSDN IDataObject::SetData)。
            // 内容はコピー済みなので、ここで直ちに解放する。
            if result.is_ok() && frelease.as_bool() {
                // SAFETY: 所有権が移った有効なSTGMEDIUM。const→mutはReleaseStgMediumの
                // C ABI都合(呼び出し後にmediumへ触れない)。
                unsafe { ReleaseStgMedium(pmedium.cast_mut()) };
            }
            result
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(windows::core::Error::from_hresult(E_NOTIMPL));
            }
            let formats = [
                hglobal_format(self.hdrop_format),
                hglobal_format(self.preferred_format),
            ];
            // SAFETY: 静的な2形式の配列からShell標準のenumeratorを生成する
            // (配列はSHCreateStdEnumFmtEtcが内部へコピーする)。
            unsafe { SHCreateStdEnumFmtEtc(&formats) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: windows::core::Ref<'_, IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(windows::core::Error::from_hresult(OLE_E_ADVISENOTSUPPORTED))
        }
    }

    /// drag継続判定。純ロジック(`super::continue_drag_decision`)へ委譲する。
    #[implement(IDropSource)]
    struct DragDropSource;

    impl IDropSource_Impl for DragDropSource_Impl {
        fn QueryContinueDrag(
            &self,
            fescapepressed: BOOL,
            grfkeystate: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            match super::continue_drag_decision(
                fescapepressed.as_bool(),
                grfkeystate.0 & MK_LBUTTON.0 != 0,
            ) {
                super::DragContinue::Cancel => DRAGDROP_S_CANCEL,
                super::DragContinue::Drop => DRAGDROP_S_DROP,
                super::DragContinue::Continue => S_OK,
            }
        }

        fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    pub(super) fn do_drag_drop(paths: &[PathBuf]) -> anyhow::Result<DragOutcome> {
        let hdrop_payload = crate::clipboard::encode_hdrop(paths)?;
        // Preferred DropEffect: sourceとして許可する効果の全bit。dragでは
        // targetは主にDoDragDropの許可effectとkey状態で効果を決めるため参考値。
        let preferred_payload = (super::DROPEFFECT_COPY_BIT
            | super::DROPEFFECT_MOVE_BIT
            | super::DROPEFFECT_LINK_BIT)
            .to_le_bytes()
            .to_vec();
        let preferred_format =
            clip_win::register_clipboard_format(crate::clipboard::PREFERRED_DROPEFFECT_FORMAT)?;
        let performed_format =
            clip_win::register_clipboard_format(super::PERFORMED_DROPEFFECT_FORMAT)?;

        let _ole = OleApartment::initialize()?;
        let performed_effect = Arc::new(Mutex::new(None));
        let data: IDataObject = DragDataObject {
            hdrop_payload,
            preferred_payload,
            hdrop_format: crate::clipboard::CF_HDROP as u16,
            preferred_format: preferred_format as u16,
            performed_format: performed_format as u16,
            performed_effect: Arc::clone(&performed_effect),
        }
        .into();
        let source: IDropSource = DragDropSource.into();

        let mut effect = DROPEFFECT(0);
        // SAFETY: `data`/`source`はこの呼び出しの間有効なCOMオブジェクト、
        // `effect`は有効な出力先。DoDragDropはdrag完了までblockする。
        let result = unsafe {
            DoDragDrop(
                &data,
                &source,
                DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                &mut effect,
            )
        };
        let dropped = if result == DRAGDROP_S_DROP {
            true
        } else if result == DRAGDROP_S_CANCEL || result.is_ok() {
            // 想定外のsuccess codeはdrop不成立(キャンセル)として扱う。
            false
        } else {
            return Err(anyhow!(
                "DoDragDrop failed: {}",
                windows::core::Error::from_hresult(result)
            ));
        };
        let performed = *performed_effect
            .lock()
            .expect("performed effect lock poisoned");
        Ok(super::resolve_outcome(dropped, effect.0, performed))
    }
}

#[cfg(not(windows))]
pub fn perform_drag(_paths: &[PathBuf]) -> anyhow::Result<DragOutcome> {
    bail!("OLE drag-and-drop is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_cancels_drag_even_if_button_is_down() {
        assert_eq!(continue_drag_decision(true, true), DragContinue::Cancel);
        assert_eq!(continue_drag_decision(true, false), DragContinue::Cancel);
    }

    #[test]
    fn releasing_left_button_drops() {
        assert_eq!(continue_drag_decision(false, false), DragContinue::Drop);
    }

    #[test]
    fn drag_continues_while_button_held() {
        assert_eq!(continue_drag_decision(false, true), DragContinue::Continue);
    }

    #[test]
    fn format_supported_requires_matching_format_and_hglobal_content() {
        const CF: u16 = 15;
        assert!(format_supported(CF, TYMED_HGLOBAL_BIT, DVASPECT_CONTENT_BIT, CF));
        // tymedはbit集合として問い合わせられる。
        assert!(format_supported(CF, TYMED_HGLOBAL_BIT | 4, DVASPECT_CONTENT_BIT, CF));
        assert!(!format_supported(CF, TYMED_HGLOBAL_BIT, DVASPECT_CONTENT_BIT, CF + 1));
        assert!(!format_supported(CF, 4, DVASPECT_CONTENT_BIT, CF)); // TYMED_ISTREAMのみ
        assert!(!format_supported(CF, TYMED_HGLOBAL_BIT, 4, CF)); // DVASPECT_ICONのみ
    }

    #[test]
    fn cancelled_drag_ignores_effects() {
        assert_eq!(
            resolve_outcome(false, DROPEFFECT_MOVE_BIT, Some(DROPEFFECT_MOVE_BIT)),
            DragOutcome::Cancelled
        );
    }

    #[test]
    fn copy_drop_reports_no_move() {
        assert_eq!(
            resolve_outcome(true, DROPEFFECT_COPY_BIT, None),
            DragOutcome::Dropped {
                effect: DropEffect::Copy,
                move_reported: false,
            }
        );
        // link等、後始末不要な効果もCopyへ畳む。
        assert_eq!(
            resolve_outcome(true, DROPEFFECT_LINK_BIT, Some(DROPEFFECT_LINK_BIT)),
            DragOutcome::Dropped {
                effect: DropEffect::Copy,
                move_reported: false,
            }
        );
    }

    #[test]
    fn move_is_reported_from_return_value_or_performed_effect() {
        // 戻り値のみで報告(unoptimized move)。
        assert_eq!(
            resolve_outcome(true, DROPEFFECT_MOVE_BIT, None),
            DragOutcome::Dropped {
                effect: DropEffect::Move,
                move_reported: true,
            }
        );
        // 戻り値はNONEでも"Performed DropEffect"で報告(Explorerの最適化move)。
        assert_eq!(
            resolve_outcome(true, 0, Some(DROPEFFECT_MOVE_BIT)),
            DragOutcome::Dropped {
                effect: DropEffect::Move,
                move_reported: true,
            }
        );
    }

    #[test]
    fn drop_without_any_effect_report_folds_to_copy() {
        assert_eq!(
            resolve_outcome(true, 0, Some(0)),
            DragOutcome::Dropped {
                effect: DropEffect::Copy,
                move_reported: false,
            }
        );
    }
}
