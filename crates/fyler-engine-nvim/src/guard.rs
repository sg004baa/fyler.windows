//! 事故防止(DESIGN.md「事故防止(想定外画面遷移の防止)」)。
//!
//! 防御対象は「知らないうちに想定外の画面状態・保存経路に入ること」のみ。
//! `:lua` 等の意図的な破壊操作への防御はスコープ外(脅威モデル参照)。
//! cmdline(`:` / `/`)はユーザーに開放する(`:%s` バルクリネームは中核機能)。

use anyhow::Context;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Buffer, Neovim};
use rmpv::Value;
use tokio::process::ChildStdin;

type NvimWriter = Compat<ChildStdin>;
type Nvim = Neovim<NvimWriter>;

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
pub(crate) async fn install_guards(
    nvim: &Nvim,
    buffer: &Buffer<NvimWriter>,
    channel_id: i64,
) -> anyhow::Result<()> {
    let buffer_number = buffer
        .get_number()
        .await
        .map_err(|error| anyhow::anyhow!("fylerバッファ番号の取得に失敗しました: {error}"))?;
    let write_events = HANDLED_WRITE_AUTOCMDS
        .iter()
        .map(|event| Value::from(*event))
        .collect();

    nvim.exec_lua(
        r#"
local buffer, channel, write_events = ...

vim.bo[buffer].buftype = "acwrite"
vim.bo[buffer].bufhidden = "hide"
vim.bo[buffer].swapfile = false
vim.bo[buffer].expandtab = false

local group = vim.api.nvim_create_augroup("fyler_guards", { clear = true })

vim.keymap.set({ "n", "x" }, "<CR>", function()
  local line = vim.api.nvim_win_get_cursor(0)[1] - 1
  vim.rpcnotify(channel, "fyler_open", line)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "^", function()
  vim.rpcnotify(channel, "fyler_parent")
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "g.", function()
  vim.rpcnotify(channel, "fyler_toggle_hidden")
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "gy", function()
  vim.rpcnotify(channel, "fyler_yank_path", vim.api.nvim_win_get_cursor(0)[1] - 1)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "gd", function()
  vim.rpcnotify(channel, "fyler_navigate_into", vim.api.nvim_win_get_cursor(0)[1] - 1)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "?", function()
  vim.rpcnotify(channel, "fyler_help")
end, { buffer = buffer, silent = true, nowait = true })

vim.api.nvim_buf_create_user_command(buffer, "FylerBookmark", function(opts)
  vim.rpcnotify(channel, "fyler_bookmark", opts.args)
end, { nargs = "?" })
vim.cmd([[cnoreabbrev <buffer> <expr> b (getcmdtype() == ':' && getcmdline() ==# 'b') ? 'FylerBookmark' : 'b']])

vim.api.nvim_buf_create_user_command(buffer, "FylerCd", function(opts)
  vim.rpcnotify(channel, "fyler_cd", opts.args)
end, { nargs = "?", complete = "dir" })
vim.cmd([[cnoreabbrev <buffer> <expr> cd (getcmdtype() == ':' && getcmdline() ==# 'cd') ? 'FylerCd' : 'cd']])

for _, lhs in ipairs({ "gf", "gF", "<C-]>" }) do
  vim.keymap.set({ "n", "x" }, lhs, function()
    vim.rpcnotify(channel, "fyler_action_blocked", lhs)
  end, { buffer = buffer, silent = true, nowait = true })
end

for _, event in ipairs(write_events) do
  local event_name = event
  vim.api.nvim_create_autocmd(event_name, {
    group = group,
    buffer = buffer,
    callback = function()
      if event_name == "BufWriteCmd" then
        vim.rpcnotify(channel, "fyler_commit_requested")
        return
      end

      vim.rpcnotify(channel, "fyler_write_blocked", event_name)
      error("fyler: unsupported write path: " .. event_name)
    end,
  })
end

vim.api.nvim_create_autocmd("BufEnter", {
  group = group,
  callback = function(args)
    if args.buf == buffer or not vim.api.nvim_buf_is_valid(buffer) then
      return
    end

    local unexpected = args.buf
    vim.schedule(function()
      if not vim.api.nvim_buf_is_valid(buffer) then
        return
      end
      if vim.api.nvim_get_current_buf() == unexpected then
        vim.api.nvim_set_current_buf(buffer)
      end
      if unexpected ~= buffer and vim.api.nvim_buf_is_valid(unexpected) then
        pcall(vim.api.nvim_buf_delete, unexpected, { force = true })
      end
      vim.rpcnotify(channel, "fyler_unexpected_buffer", unexpected)
    end)
  end,
})
"#,
        vec![
            Value::from(buffer_number),
            Value::from(channel_id),
            Value::Array(write_events),
        ],
    )
    .await
    .map_err(|error| anyhow::anyhow!("事故防止設定の導入に失敗しました: {error}"))
    .context("Neovim guard初期化エラー")?;

    Ok(())
}
