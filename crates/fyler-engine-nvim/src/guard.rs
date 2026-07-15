//! 事故防止(DESIGN.md「事故防止(想定外画面遷移の防止)」)。
//!
//! 防御対象は「知らないうちに想定外の画面状態・保存経路に入ること」のみ。
//! `:lua` 等の意図的な破壊操作への防御はスコープ外(脅威モデル参照)。
//! cmdline(`:` / `/`)はユーザーに開放する(`:%s` バルクリネームは中核機能)。

use anyhow::Context;
use fyler_core::editor::{Key, Modifiers};
use fyler_core::keymap::{EditorAction, KeyBinding};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::{Buffer, Neovim};
use rmpv::Value;
use tokio::process::ChildStdin;

use crate::translate::{sequence_to_lhs, to_nvim_keycodes};

type NvimWriter = Compat<ChildStdin>;
type Nvim = Neovim<NvimWriter>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BindingPayload {
    kind: &'static str,
    arg: Option<&'static str>,
    modes: &'static [&'static str],
}

fn binding_payload(action: EditorAction) -> BindingPayload {
    use EditorAction::*;
    let (kind, arg, modes): (_, _, &'static [&'static str]) = match action {
        Activate => ("activate", None, &["n", "x"]),
        NavigateParent => ("navigate_parent", None, &["n"]),
        NavigateInto => ("navigate_into", None, &["n"]),
        ToggleHidden => ("toggle_hidden", None, &["n"]),
        FoldClose => ("fold", Some("close"), &["n"]),
        FoldOpen => ("fold", Some("open"), &["n"]),
        FoldToggle => ("fold", Some("toggle"), &["n"]),
        FoldCloseRecursive => ("fold", Some("close_rec"), &["n"]),
        FoldOpenRecursive => ("fold", Some("open_rec"), &["n"]),
        FoldCloseAll => ("fold", Some("close_all"), &["n"]),
        FoldOpenAll => ("fold", Some("open_all"), &["n"]),
        FilePicker => ("file_picker", None, &["n"]),
        YankPath => ("yank_path", None, &["n"]),
        OpenWith => ("open_with", None, &["n"]),
        TransferMove => ("transfer", Some("move"), &["n", "x"]),
        TransferCopy => ("transfer", Some("copy"), &["n", "x"]),
        ToggleDockFocus => ("dock_focus", None, &["n"]),
        Help => ("help", None, &["n"]),
        PaneSplitHorizontal => ("pane", Some("split_horizontal"), &["n"]),
        PaneSplitVertical => ("pane", Some("split_vertical"), &["n"]),
        PaneFocusLeft => ("pane", Some("focus_left"), &["n"]),
        PaneFocusDown => ("pane", Some("focus_down"), &["n"]),
        PaneFocusUp => ("pane", Some("focus_up"), &["n"]),
        PaneFocusRight => ("pane", Some("focus_right"), &["n"]),
        PaneFocusNext => ("pane", Some("focus_next"), &["n"]),
        PaneFocusPrevious => ("pane", Some("focus_previous"), &["n"]),
        PaneClose => ("pane", Some("close"), &["n"]),
    };
    BindingPayload { kind, arg, modes }
}

fn payload_value(payload: BindingPayload) -> Value {
    let mut fields = vec![
        (Value::from("kind"), Value::from(payload.kind)),
        (
            Value::from("modes"),
            Value::Array(
                payload
                    .modes
                    .iter()
                    .map(|mode| Value::from(*mode))
                    .collect(),
            ),
        ),
    ];
    if let Some(arg) = payload.arg {
        fields.push((Value::from("arg"), Value::from(arg)));
    }
    Value::Map(fields)
}

fn is_ctrl_w(binding: &KeyBinding) -> bool {
    binding.sequence.0.first().is_some_and(|stroke| {
        stroke.key == Key::Char('w')
            && stroke.mods
                == Modifiers {
                    ctrl: true,
                    alt: false,
                    shift: false,
                }
    })
}

#[derive(Default)]
struct TrieNode {
    leaf: Option<BindingPayload>,
    children: std::collections::BTreeMap<String, TrieNode>,
}

fn binding_values(bindings: &[KeyBinding]) -> (Value, Value) {
    let mut normal = Vec::new();
    let mut ctrl_w = TrieNode::default();
    for binding in bindings {
        let payload = binding_payload(binding.action);
        if is_ctrl_w(binding) {
            let mut node = &mut ctrl_w;
            for stroke in &binding.sequence.0[1..] {
                node = node.children.entry(to_nvim_keycodes(stroke)).or_default();
            }
            debug_assert!(node.leaf.is_none() && node.children.is_empty());
            if node.leaf.is_none() && node.children.is_empty() {
                node.leaf = Some(payload);
            }
        } else {
            let mut value = match payload_value(payload) {
                Value::Map(fields) => fields,
                _ => unreachable!(),
            };
            value.push((
                Value::from("lhs"),
                Value::from(sequence_to_lhs(&binding.sequence)),
            ));
            normal.push(Value::Map(value));
        }
    }
    (Value::Array(normal), trie_value(ctrl_w))
}

fn trie_value(node: TrieNode) -> Value {
    if let Some(leaf) = node.leaf {
        return payload_value(leaf);
    }
    Value::Map(
        node.children
            .into_iter()
            .map(|(key, child)| (Value::from(key), trie_value(child)))
            .collect(),
    )
}

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
    bindings: &[KeyBinding],
) -> anyhow::Result<()> {
    let buffer_number = buffer
        .get_number()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to get fyler buffer number: {error}"))?;
    let write_events = HANDLED_WRITE_AUTOCMDS
        .iter()
        .map(|event| Value::from(*event))
        .collect();
    let (binding_values, ctrl_w_trie) = binding_values(bindings);

    nvim.exec_lua(
        r#"
local buffer, channel, write_events, bindings, ctrlw_trie = ...

vim.bo[buffer].buftype = "acwrite"
vim.bo[buffer].bufhidden = "hide"
vim.bo[buffer].swapfile = false
vim.bo[buffer].expandtab = false

local group = vim.api.nvim_create_augroup("fyler_guards", { clear = true })

local function line_depth(line)
  local prefix = line:match("^/%d+ ") or ""
  local rest = line:sub(#prefix + 1)
  local tabs = rest:match("^\t*")
  return #tabs
end

local function name_start_col(line)
  local prefix = line:match("^/%d+ ") or ""
  local tabs = line_depth(line)
  return #prefix + #tabs
end

local function open_line_with_current_depth(command)
  local row = vim.api.nvim_win_get_cursor(0)[1] - 1
  local line = vim.api.nvim_buf_get_lines(buffer, row, row + 1, false)[1] or ""
  return command .. string.rep("\t", line_depth(line))
end

vim.keymap.set("n", "o", function()
  return open_line_with_current_depth("o")
end, { buffer = buffer, expr = true, silent = true })

vim.keymap.set("n", "O", function()
  return open_line_with_current_depth("O")
end, { buffer = buffer, expr = true, silent = true })

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

local function dispatch(binding)
  local line = vim.api.nvim_win_get_cursor(0)[1] - 1
  if binding.kind == "activate" then
    vim.rpcnotify(channel, "fyler_open", line)
  elseif binding.kind == "navigate_parent" then
    vim.rpcnotify(channel, "fyler_parent")
  elseif binding.kind == "navigate_into" then
    vim.rpcnotify(channel, "fyler_navigate_into", line)
  elseif binding.kind == "toggle_hidden" then
    vim.rpcnotify(channel, "fyler_toggle_hidden")
  elseif binding.kind == "fold" then
    vim.rpcnotify(channel, "fyler_fold", binding.arg, line)
  elseif binding.kind == "file_picker" then
    vim.rpcnotify(channel, "fyler_open_picker")
  elseif binding.kind == "yank_path" then
    vim.rpcnotify(channel, "fyler_yank_path", line)
  elseif binding.kind == "open_with" then
    vim.rpcnotify(channel, "fyler_open_with", line)
  elseif binding.kind == "transfer" then
    request_transfer(binding.arg, vim.fn.mode():sub(1, 1) ~= "n")
  elseif binding.kind == "dock_focus" then
    vim.rpcnotify(channel, "fyler_dock_focus")
  elseif binding.kind == "help" then
    vim.rpcnotify(channel, "fyler_help")
  elseif binding.kind == "pane" then
    vim.rpcnotify(channel, "fyler_pane", binding.arg)
  end
end

local function make_callback(binding)
  return function()
    dispatch(binding)
  end
end

for _, binding in ipairs(bindings) do
  vim.keymap.set(binding.modes, binding.lhs, make_callback(binding), { buffer = buffer, silent = true, nowait = true })
end

vim.keymap.set("n", "<C-w>", function()
  local node = ctrlw_trie
  while true do
    local key = vim.fn.getcharstr()
    local next_node = nil
    for notation, child in pairs(node) do
      if key == vim.keycode(notation) then
        next_node = child
        break
      end
    end
    if next_node == nil then
      vim.rpcnotify(channel, "fyler_action_blocked", key)
      return
    end
    if next_node.kind ~= nil then
      dispatch(next_node)
      return
    end
    node = next_node
  end
end, { buffer = buffer, silent = true, nowait = true })

vim.api.nvim_buf_create_user_command(buffer, "FylerBookmark", function(opts)
  vim.rpcnotify(channel, "fyler_bookmark", opts.args)
end, { nargs = "?" })

vim.api.nvim_buf_create_user_command(buffer, "FylerUndo", function()
  vim.rpcnotify(channel, "fyler_undo")
end, {})

vim.api.nvim_buf_create_user_command(buffer, "FylerFeedback", function()
  vim.rpcnotify(channel, "fyler_feedback")
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

vim.api.nvim_buf_create_user_command(buffer, "FylerTerminal", function(opts)
  local line = vim.api.nvim_win_get_cursor(0)[1] - 1
  vim.rpcnotify(channel, "fyler_terminal", line, opts.args)
end, { nargs = "*", bang = true })

vim.o.wildcharm = 26
local command_aliases = { b = "FylerBookmark", cd = "FylerCd", feedback = "FylerFeedback", sort = "FylerSort", terminal = "FylerTerminal" }
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
            binding_values,
            ctrl_w_trie,
        ],
    )
    .await
    .map_err(|error| anyhow::anyhow!("Failed to install safety guards: {error}"))
    .context("Failed to initialize Neovim guard")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_maps_to_the_existing_rpc_contract() {
        use EditorAction::*;
        let cases = [
            (Activate, "activate", None, &["n", "x"][..]),
            (NavigateParent, "navigate_parent", None, &["n"][..]),
            (NavigateInto, "navigate_into", None, &["n"][..]),
            (ToggleHidden, "toggle_hidden", None, &["n"][..]),
            (FoldClose, "fold", Some("close"), &["n"][..]),
            (FoldOpen, "fold", Some("open"), &["n"][..]),
            (FoldToggle, "fold", Some("toggle"), &["n"][..]),
            (FoldCloseRecursive, "fold", Some("close_rec"), &["n"][..]),
            (FoldOpenRecursive, "fold", Some("open_rec"), &["n"][..]),
            (FoldCloseAll, "fold", Some("close_all"), &["n"][..]),
            (FoldOpenAll, "fold", Some("open_all"), &["n"][..]),
            (FilePicker, "file_picker", None, &["n"][..]),
            (YankPath, "yank_path", None, &["n"][..]),
            (OpenWith, "open_with", None, &["n"][..]),
            (TransferMove, "transfer", Some("move"), &["n", "x"][..]),
            (TransferCopy, "transfer", Some("copy"), &["n", "x"][..]),
            (ToggleDockFocus, "dock_focus", None, &["n"][..]),
            (Help, "help", None, &["n"][..]),
            (
                PaneSplitHorizontal,
                "pane",
                Some("split_horizontal"),
                &["n"][..],
            ),
            (
                PaneSplitVertical,
                "pane",
                Some("split_vertical"),
                &["n"][..],
            ),
            (PaneFocusLeft, "pane", Some("focus_left"), &["n"][..]),
            (PaneFocusDown, "pane", Some("focus_down"), &["n"][..]),
            (PaneFocusUp, "pane", Some("focus_up"), &["n"][..]),
            (PaneFocusRight, "pane", Some("focus_right"), &["n"][..]),
            (PaneFocusNext, "pane", Some("focus_next"), &["n"][..]),
            (
                PaneFocusPrevious,
                "pane",
                Some("focus_previous"),
                &["n"][..],
            ),
            (PaneClose, "pane", Some("close"), &["n"][..]),
        ];
        for (action, kind, arg, modes) in cases {
            assert_eq!(
                binding_payload(action),
                BindingPayload { kind, arg, modes },
                "{}",
                action.config_name()
            );
        }
    }

    #[test]
    fn defaults_split_into_normal_maps_and_ctrl_w_trie() {
        let bindings = fyler_core::keymap::default_bindings(fyler_core::keymap::default_leader());
        let (normal, trie) = binding_values(&bindings);
        assert_eq!(normal.as_array().unwrap().len(), 18);
        let trie = trie.as_map().unwrap();
        assert_eq!(trie.len(), 12);
        assert!(trie.iter().any(|(key, _)| key.as_str() == Some("<C-w>")));
    }
}
