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

local function name_start_col(line)
  local prefix = line:match("^/%d+ ") or ""
  local rest = line:sub(#prefix + 1)
  local tabs = rest:match("^\t*")
  return #prefix + #tabs
end

local function shift_lines(first, last, delta)
  local lines = vim.api.nvim_buf_get_lines(buffer, first, last + 1, false)
  local changed = false
  for i, line in ipairs(lines) do
    if line ~= "" then
      local prefix, rest = line:match("^(/%d+ )(.*)$")
      if not prefix then prefix, rest = "", line end
      if delta > 0 then
        lines[i] = prefix .. "\t" .. rest
        changed = true
      else
        local tabs, name = rest:match("^(\t*)(.*)$")
        if #tabs > 0 then
          lines[i] = prefix .. tabs:sub(2) .. name
          changed = true
        end
      end
    end
  end
  if changed then
    vim.api.nvim_buf_set_lines(buffer, first, last + 1, false, lines)
  end
end

_G.__fyler_shift_delta = 0
_G.__fyler_shift_op = function(_)
  local first = vim.api.nvim_buf_get_mark(0, "[")[1] - 1
  local last = vim.api.nvim_buf_get_mark(0, "]")[1] - 1
  shift_lines(first, last, _G.__fyler_shift_delta)
  vim.api.nvim_win_set_cursor(0, { first + 1, 0 })
end

for lhs, delta in pairs({ [">"] = 1, ["<"] = -1 }) do
  vim.keymap.set("n", lhs, function()
    _G.__fyler_shift_delta = delta
    vim.o.operatorfunc = "v:lua.__fyler_shift_op"
    return "g@"
  end, { buffer = buffer, expr = true, silent = true })
  vim.keymap.set("n", lhs .. lhs, function()
    _G.__fyler_shift_delta = delta
    vim.o.operatorfunc = "v:lua.__fyler_shift_op"
    return "g@_"
  end, { buffer = buffer, expr = true, silent = true })
  vim.keymap.set("x", lhs, function()
    local first = vim.fn.line("v") - 1
    local last = vim.api.nvim_win_get_cursor(0)[1] - 1
    if first > last then first, last = last, first end
    shift_lines(first, last, delta)
    vim.api.nvim_input("<Esc>")
    vim.api.nvim_win_set_cursor(0, { first + 1, 0 })
  end, { buffer = buffer, silent = true })
end

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

for lhs, op in pairs({ zc = "close", zo = "open", za = "toggle", ["zC"] = "close_rec", ["zO"] = "open_rec", ["zM"] = "close_all", ["zR"] = "open_all" }) do
  vim.keymap.set("n", lhs, function()
    vim.rpcnotify(channel, "fyler_fold", op, vim.api.nvim_win_get_cursor(0)[1] - 1)
  end, { buffer = buffer, silent = true, nowait = true })
end
vim.keymap.set("n", "g/", function()
  vim.rpcnotify(channel, "fyler_open_picker")
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "gy", function()
  vim.rpcnotify(channel, "fyler_yank_path", vim.api.nvim_win_get_cursor(0)[1] - 1)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "go", function()
  vim.rpcnotify(channel, "fyler_open_with", vim.api.nvim_win_get_cursor(0)[1] - 1)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "gd", function()
  vim.rpcnotify(channel, "fyler_navigate_into", vim.api.nvim_win_get_cursor(0)[1] - 1)
end, { buffer = buffer, silent = true, nowait = true })

local function request_transfer(kind, visual)
  local cursor = vim.api.nvim_win_get_cursor(0)[1] - 1
  local first, last = cursor, cursor
  if visual then
    local anchor = vim.fn.line("v") - 1
    first = math.min(anchor, cursor)
    last = math.max(anchor, cursor)
  end
  vim.rpcnotify(channel, "fyler_transfer", kind, first, last)
end

vim.keymap.set("n", "gm", function()
  request_transfer("move", false)
end, { buffer = buffer, silent = true, nowait = true })
vim.keymap.set("x", "gm", function()
  request_transfer("move", true)
end, { buffer = buffer, silent = true, nowait = true })
vim.keymap.set("n", "gc", function()
  request_transfer("copy", false)
end, { buffer = buffer, silent = true, nowait = true })
vim.keymap.set("x", "gc", function()
  request_transfer("copy", true)
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "?", function()
  vim.rpcnotify(channel, "fyler_help")
end, { buffer = buffer, silent = true, nowait = true })

vim.keymap.set("n", "<C-w>", function()
  local key = vim.fn.getcharstr()
  local actions = {
    s = "split_horizontal",
    S = "split_horizontal",
    v = "split_vertical",
    h = "focus_left",
    j = "focus_down",
    k = "focus_up",
    l = "focus_right",
    w = "focus_next",
    p = "focus_previous",
    q = "close",
    c = "close",
  }
  local action = actions[key]
  if key == vim.keycode("<C-w>") then
    action = "focus_next"
  end
  if action then
    vim.rpcnotify(channel, "fyler_pane", action)
  else
    vim.rpcnotify(channel, "fyler_action_blocked", key)
  end
end, { buffer = buffer, silent = true, nowait = true })

vim.api.nvim_buf_create_user_command(buffer, "FylerBookmark", function(opts)
  vim.rpcnotify(channel, "fyler_bookmark", opts.args)
end, { nargs = "?" })

vim.api.nvim_buf_create_user_command(buffer, "FylerUndo", function()
  vim.rpcnotify(channel, "fyler_undo")
end, {})

vim.api.nvim_buf_create_user_command(buffer, "FylerCd", function(opts)
  vim.rpcnotify(channel, "fyler_cd", opts.args)
end, { nargs = "?", complete = "dir" })

local sort_keys = { "name", "date", "size", "ext" }
vim.api.nvim_buf_create_user_command(buffer, "FylerSort", function(opts)
  local arg = opts.args
  if opts.bang and arg ~= "" then arg = arg .. "!" end
  vim.rpcnotify(channel, "fyler_sort", arg)
end, {
  nargs = "?",
  bang = true,
  complete = function(arg_lead)
    return vim.tbl_filter(function(key)
      return vim.startswith(key, arg_lead)
    end, sort_keys)
  end,
})

vim.o.wildcharm = 26
local command_aliases = { b = "FylerBookmark", cd = "FylerCd", sort = "FylerSort" }
-- nvim_paste経由ではcnoreabbrevが展開されないため、実行/補完直前に先頭語を正式コマンドへ書き換える。
local function rewrite_command_alias()
  if vim.fn.getcmdtype() ~= ":" then return end
  local line = vim.fn.getcmdline()
  local head, bang, rest = line:match("^(%a+)(!?)(.*)$")
  local target = head and command_aliases[head]
  if target and (rest == "" or rest:match("^%s")) then
    local pos = vim.fn.getcmdpos()
    vim.fn.setcmdline(target .. bang .. rest, pos + #target - #head)
  end
end
vim.keymap.set("c", "<Tab>", function()
  rewrite_command_alias()
  return "<C-Z>"
end, { buffer = buffer, expr = true })
vim.keymap.set("c", "<CR>", function()
  rewrite_command_alias()
  return "<CR>"
end, { buffer = buffer, expr = true })

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

vim.api.nvim_create_autocmd({ "CursorMoved", "CursorMovedI" }, {
  group = group,
  buffer = buffer,
  callback = function()
    local pos = vim.api.nvim_win_get_cursor(0)
    local line = vim.api.nvim_buf_get_lines(buffer, pos[1] - 1, pos[1], false)[1] or ""
    local min_col = name_start_col(line)
    if pos[2] < min_col and #line > min_col then
      vim.api.nvim_win_set_cursor(0, { pos[1], min_col })
    end
  end,
})

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
