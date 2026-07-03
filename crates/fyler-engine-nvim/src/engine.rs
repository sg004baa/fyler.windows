//! NvimEngine本体: snapshot + command channel型の `EditorEngine` 実装。

use std::sync::Arc;

use arc_swap::ArcSwap;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorEvent, EditorSnapshot};
use tokio::sync::mpsc;

use crate::spawn::NvimConfig;

/// `EditorEngine` のNeovim実装。
///
/// - GUIスレッドから見えるのは「snapshotの読み取り」と「コマンド送信」だけ。
///   RPCの往復はすべてバックグラウンドのtokioタスク(エンジンタスク)が行う
/// - snapshotは [`ArcSwap`] で原子的に差し替える(GUIはロックなしで読む)
pub struct NvimEngine {
    snapshot: ArcSwap<EditorSnapshot>,
    cmd_tx: mpsc::UnboundedSender<EditorCommand>,
}

impl EditorEngine for NvimEngine {
    fn send(&self, cmd: EditorCommand) -> anyhow::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("NvimEngineタスクが終了しています"))
    }

    fn snapshot(&self) -> Arc<EditorSnapshot> {
        self.snapshot.load_full()
    }
}

impl NvimEngine {
    /// nvimを起動し、エンジンタスクを開始する(M1)。
    ///
    /// 実装契約(DESIGN.md「nvim統合の詳細」):
    ///
    /// 1. **プロセス起動**: `config.nvim_exe` を [`crate::spawn::NVIM_ARGS`] で起動。
    ///    Windowsでは必ず `creation_flags(CREATE_NO_WINDOW)` を付ける
    /// 2. **バッファ設定**: バッファ名は `filer://C:/Users/...` 形式の架空URI
    ///    ([`crate::guard::BUFFER_URI_SCHEME`])、`buftype=acwrite`
    /// 3. **同期経路**:
    ///    - 行内容: `nvim_buf_attach` のプッシュ通知(`nvim_buf_lines_event`)
    ///    - mode / cursor / changedtick: キー送信ごとに `nvim_call_atomic` で
    ///      一括取得し、[`EditorSnapshot`] として原子的に更新(revisionはRust側で単調増加)
    /// 4. **UI attach**: 起動後に `nvim_ui_attach` を最小グリッドサイズで後付け実行。
    ///    有効化するextは `ext_cmdline` と `ext_messages` の2つだけ
    ///    (ext_messagesがないと `E486` 等がgridに描かれて見えない)。
    ///    grid系描画イベントはすべて無視。`ext_popupmenu` は補完UI実装時に追加
    /// 5. **事故防止**: [`crate::guard`] のautocmd・remapを導入
    /// 6. **コマンド処理**: `cmd_rx` から受けた [`EditorCommand`] を処理する。
    ///    `Key` は [`crate::translate::to_nvim_keycodes`] → `nvim_input`、
    ///    `Text` は `nvim_input` でなくバッファ挿入APIまたは `nvim_paste`(M0で検証)、
    ///    `Paste` は `nvim_paste`、`RequestCommit` は `:w` 相当、
    ///    `Undo`/`Redo` は `u`/`<C-r>` 相当
    /// 7. **イベント**: BufWriteCmd等のrpcnotify → `EditorEvent::CommitRequested`、
    ///    ext_cmdline → `CmdlineShow/CmdlineHide`、ext_messages → `Message`、
    ///    プロセス終了検知 → `EngineCrashed` として `event_tx` へ流す
    ///
    /// 戻り値: エンジン本体と、GUI/app層が受けるイベントストリーム。
    pub async fn start(
        config: NvimConfig,
    ) -> anyhow::Result<(Arc<Self>, mpsc::UnboundedReceiver<EditorEvent>)> {
        todo!("M1: nvim spawn + RPC疎通 + snapshot同期(上記の実装契約参照)")
    }
}
