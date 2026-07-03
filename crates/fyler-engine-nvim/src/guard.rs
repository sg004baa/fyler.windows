//! 事故防止(DESIGN.md「事故防止(想定外画面遷移の防止)」)。
//!
//! 防御対象は「知らないうちに想定外の画面状態・保存経路に入ること」のみ。
//! `:lua` 等の意図的な破壊操作への防御はスコープ外(脅威モデル参照)。
//! cmdline(`:` / `/`)はユーザーに開放する(`:%s` バルクリネームは中核機能)。

/// fylerバッファの架空URIスキーム。バッファ名は `filer://C:/Users/...` 形式。
pub const BUFFER_URI_SCHEME: &str = "filer://";

/// fylerバッファの `buftype`。`acwrite` により `:w` が `BufWriteCmd` を発火する。
pub const BUFTYPE: &str = "acwrite";

/// 網羅的にハンドルする保存系autocmdイベント。
///
/// - `BufWriteCmd`: `:w` → 保存状態機械(`fyler_core::save`)の入口
/// - `FileWriteCmd` / `FileAppendCmd`: 部分書き込み・別名書き込み
/// - `BufFilePre`: `:file` / `:saveas`
///
/// BufWriteCmd以外は同一経路へ誘導するか、明示的にエラーにする(黙って無視しない)。
pub const HANDLED_WRITE_AUTOCMDS: &[&str] =
    &["BufWriteCmd", "FileWriteCmd", "FileAppendCmd", "BufFilePre"];

/// 事故防止のremap・autocmdをfylerバッファへ導入する(M1)。
///
/// 実装契約:
/// - `<CR>`(ファイルを開く)等のアクションはバッファローカルmapで
///   `rpcnotify` に差し替える
/// - 想定外のバッファ(`gf` や `:e 実パス` によるもの)が開かれたことを
///   autocmd(BufEnter等)で検知したら即座に閉じ、fylerバッファへ戻す
/// - [`HANDLED_WRITE_AUTOCMDS`] を漏れなく登録する
/// - `BufWriteCmd` はハンドラ自身が書き込みを完了させる前提のイベントなので、
///   rpcnotify後の完了扱い(modified等)は保存状態機械の指示に従う
///
/// シグネチャはnvim-rsのクライアント型を受ける形で実装時に確定する
/// (nvim-rsの型はこのクレートの外に出さないこと)。
pub fn install_guards() {
    todo!("M1: バッファローカルmap + autocmd導入")
}
