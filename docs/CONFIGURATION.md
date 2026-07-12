# Configuration

fyler reads user settings from `config.toml`. Settings are loaded only at startup, so restart
fyler after changing the file.

## Configuration file location

| Environment | Path |
|---|---|
| Windows | `%APPDATA%\fyler\config.toml` |
| Linux and other platforms | `$XDG_CONFIG_HOME/fyler/config.toml` |
| When `XDG_CONFIG_HOME` is unset | `~/.config/fyler/config.toml` |

If the file does not exist, fyler starts with the defaults. fyler only reads `config.toml`; it
does not create or rewrite it.

For development and testing, `FYLER_CONFIG_DIR` overrides the configuration directory.
`FYLER_NVIM_EXE` selects the Neovim executable used by fyler.

When a value has the wrong type or is otherwise invalid, fyler falls back where possible and
ignores only the affected setting. Warnings are shown as in-app messages.

## Complete example

```toml
# Show hidden files at startup
show_hidden = false

# Put directories first and sort by name in ascending order
sort = "dirs_first"
sort_key = "name"
sort_reverse = false
# Restore the last normally closed pane session
restore_session = true


# External terminal emulator used by :terminal
terminal = "auto"

# Show every operation in confirmation dialogs
confirm_detail = "full"

# Japanese fallback font and vertical alignment adjustment
# TOML literal strings are convenient for Windows paths because backslashes need no escaping
font = 'C:\Windows\Fonts\meiryo.ttc'
font_y_offset_factor = 0.12

# Use "nerd" for Nerd Font icons or "ascii" for portable icons
icons = "ascii"

# Leader must be one unmodified key. The default is Space.
leader = "Space"

[bookmarks]
home = 'C:\Users\me'
projects = 'D:\projects'

[keymap.normal]
"Leader f" = "file_picker"
"Leader h" = "toggle_hidden"
"g /" = "none"
"Ctrl+W x" = "pane_focus_next"
```

## Settings reference

| Setting | Type | Default | Values and behavior |
|---|---|---|---|
| `show_hidden` | boolean | `false` | Show hidden files at startup |
| `sort` | string | `"dirs_first"` | `"dirs_first"` or `"mixed"` |
| `sort_key` | string | `"name"` | `"name"`, `"date"`, `"size"`, or `"ext"` |
| `sort_reverse` | boolean | `false` | Reverse the selected sort key |
| `restore_session` | boolean | `true` | Restore pane layout, roots, cursor hints, folds, and per-pane display settings |
| `terminal` | string | `"auto"` | `"auto"`, `"windows_terminal"`, `"powershell"`, or `"cmd"` |
| `confirm_detail` | string | `"full"` | `"full"` or `"summary"` |
| `font` | string | unset | Absolute path to a fallback font |
| `font_y_offset_factor` | number | `0.12` | Downward CJK font offset as a font-size ratio; `0` disables it |
| `icons` | string | `"ascii"` | `"ascii"` or `"nerd"` |
| `leader` | string | `"Space"` | One unmodified key used to expand `Leader` bindings |
| `[bookmarks]` | table | empty | Bookmark names mapped to absolute paths |
| `[keymap.normal]` | table | built-in keymap | Key sequences mapped to action names |

### Display and sorting

`show_hidden = true` shows dot-prefixed entries and entries with the Windows hidden attribute at
startup. The `toggle_hidden` action (`g .` by default) can also change this while fyler is running.

`sort` controls how directories and files are grouped:

- `dirs_first`: place all directories before files
- `mixed`: sort directories and files together

`sort_key` controls comparison within those groups:

- `name`: case-insensitive natural order with numeric segments compared numerically
- `date`: modification time
- `size`: file size
- `ext`: file extension

`sort_reverse = true` reverses the selected key. Directory grouping remains controlled separately
by `sort`. At runtime, use `:sort name|date|size|ext`; add `!` to the command for descending order.
### Session and window restoration

With `restore_session = true`, a normally closed fyler window writes `session.toml` beside
`config.toml` using schema version 1. The session contains only display state: the binary pane
layout and split ratios, each pane's root, the active pane, root-relative cursor and fold hints,
per-pane hidden-file and sorting settings, and native window size, position, and maximized state.
Dirty buffer text, editor mode, dialogs, in-flight apply/transfer state, clipboard data,
Neovim/RPC state, and undo transactions are never stored in the session.

An explicit command-line root always starts one pane and takes precedence over `session.toml`.
`restore_session = false` also starts one pane without reading the session. Invalid TOML and
unknown schema versions produce a warning and use the default root. If one saved root is missing
or inaccessible, fyler tries its nearest available ancestor and then the default root without
discarding other valid panes. Invalid data above the four-pane limit is pruned to four panes.

Session writes use a same-directory temporary file, flush it, and atomically rename it only during
normal shutdown. A truncated temporary write therefore does not replace the previous session.
Window geometry follows the same normal-shutdown atomic session contract. Before a persisted
window exists, fyler opens centered at 70% of the current display's width and height.


### External terminal

`terminal` selects the external terminal emulator opened by `:terminal`. The command uses the
selected directory as its working directory. Supported values are `"auto"` (the default),
`"windows_terminal"`, `"powershell"`, and `"cmd"`. On Windows, `"auto"` tries Windows Terminal,
PowerShell, and cmd in that order. Non-Windows builds use `x-terminal-emulator` for development.

### Confirmation dialogs

`confirm_detail` controls how save and transfer operations appear before they are applied:

- `full`: show every operation
- `summary`: summarize large plans by operation type

### Fonts and icons

`font` accepts only an absolute font-file path. Relative paths are ignored with a warning. TOML
literal strings keep Windows paths readable:

```toml
font = 'C:\Windows\Fonts\meiryo.ttc'
```

`font_y_offset_factor` moves CJK glyphs downward by a ratio of the font size when their metrics
make them appear too high. Set it to `0` to disable the adjustment.

`icons = "nerd"` uses Nerd Font glyphs. Use `"ascii"` when the display font does not provide
those glyphs.

### Bookmarks

Define bookmark names and absolute paths under `[bookmarks]`. fyler preserves their definition
order when displaying the bookmark list.

```toml
[bookmarks]
home = 'C:\Users\me'
work = 'D:\work'
downloads = 'C:\Users\me\Downloads'
```

Use `:b` to list bookmarks and `:b home` to open one. A unique name prefix also works. Relative
paths and non-string values are ignored with a warning.

fyler stores up to ten recently used roots in `recent.toml` in the same configuration directory
and includes them in the `:b` candidates. fyler manages this file, so it normally should not be
edited manually.

## Keymap

Keymaps use engine-independent notation. Neovim notation such as `<C-w>` and `<CR>` is not
accepted. `[keymap.normal]` is currently the only supported section. Unsupported sections such as
`[keymap.visual]` are ignored with a warning.

User entries are applied on top of the built-in keymap. Assigning an action replaces the binding
for that sequence. Assigning `"none"` removes it.

```toml
leader = "Space"

[keymap.normal]
"Leader f" = "file_picker" # Expands to Space f
"g d" = "help"             # Reassign an existing sequence
"g ." = "none"             # Remove an existing binding
```

`activate`, `transfer_move`, and `transfer_copy` are mapped in both Normal and Visual modes. All
other actions are mapped in Normal mode.

### Key notation

Separate strokes with spaces and join modifiers within a stroke with `+`:

```toml
[keymap.normal]
"g d" = "navigate_into"
"Ctrl+W v" = "pane_split_vertical"
"Ctrl+Alt+F5" = "file_picker"
"Leader f" = "file_picker"
```

Modifier and named-key names are case-insensitive. Printable characters are case-sensitive.

- Modifiers: `Ctrl`, `Alt`, `Shift`
- Named keys: `Enter`, `Esc`, `Backspace`, `Tab`, `Delete`, `Space`
- Navigation keys: `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`
- Function keys: `F1` through `F12`
- Leader reference: `Leader`
- Any other single printable character, such as `g`, `?`, or `V`

Rules and limitations:

- `V` is an uppercase character and is distinct from `v`.
- Modified ASCII letters are normalized to lowercase, so `Ctrl+W` and `Ctrl+w` are equivalent.
- Do not apply `Shift` to a printable character. Write `V`, not `Shift+v`.
- `leader` itself must be one unmodified key.
- An invalid `leader` falls back to `Space` when expanding `Leader` bindings.
- A standalone `Ctrl+W` cannot be bound.
- Two `Ctrl+W` sequences cannot be configured when either is a true prefix of the other.
- Invalid keys, unknown actions, duplicates, and `none` entries with no binding to remove produce
  warnings.
- Runtime reload is not supported; restart fyler after changing the keymap.

### Available actions

| Action | Description | Default key |
|---|---|---|
| `activate` | Toggle a directory or open a file | `Enter` |
| `navigate_parent` | Go to the parent directory | `^` |
| `navigate_into` | Enter the selected directory | `g d` |
| `toggle_hidden` | Toggle hidden files | `g .` |
| `fold_close` | Collapse a directory | `z c` |
| `fold_open` | Expand a directory | `z o` |
| `fold_toggle` | Toggle a directory fold | `z a` |
| `fold_close_recursive` | Recursively collapse descendants | `z C` |
| `fold_open_recursive` | Recursively expand descendants | `z O` |
| `fold_close_all` | Collapse all directories | `z M` |
| `fold_open_all` | Expand all directories | `z R` |
| `file_picker` | Find a file | `g /` |
| `yank_path` | Copy the selected path | `g y` |
| `open_with` | Choose an application and open the entry | `g o` |
| `transfer_move` | Move entries to another pane | `g m` |
| `transfer_copy` | Copy entries to another pane | `g c` |
| `help` | Show help | `?` |
| `pane_split_horizontal` | Split the pane horizontally | `Ctrl+W s`, `Ctrl+W S` |
| `pane_split_vertical` | Split the pane vertically | `Ctrl+W v` |
| `pane_focus_left` | Focus the pane to the left | `Ctrl+W h` |
| `pane_focus_down` | Focus the pane below | `Ctrl+W j` |
| `pane_focus_up` | Focus the pane above | `Ctrl+W k` |
| `pane_focus_right` | Focus the pane to the right | `Ctrl+W l` |
| `pane_focus_next` | Focus the next pane | `Ctrl+W w`, `Ctrl+W Ctrl+W` |
| `pane_focus_previous` | Focus the previous pane | `Ctrl+W p` |
| `pane_close` | Close the current pane | `Ctrl+W q`, `Ctrl+W c` |

`none` is not an action. It is a special value that removes the binding for the specified key
sequence.

The help dialog opened with `?` is generated from the resolved built-in and user bindings. It
omits actions with no remaining bindings and reflects added or reassigned keys.
