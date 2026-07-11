# Configuration

fylerのユーザー設定は`config.toml`に記述します。設定は起動時にだけ読み込まれるため、
変更を反映するにはfylerを再起動してください。

## 設定ファイルの場所

| 環境 | パス |
|---|---|
| Windows | `%APPDATA%\fyler\config.toml` |
| Linuxなど | `$XDG_CONFIG_HOME/fyler/config.toml` |
| `XDG_CONFIG_HOME`未設定時 | `~/.config/fyler/config.toml` |

ファイルが存在しない場合は、すべて既定値で起動します。fylerは`config.toml`を
読み取るだけで、作成や書き換えは行いません。

開発・テスト用途では、環境変数`FYLER_CONFIG_DIR`で設定ディレクトリを変更できます。
また、`FYLER_NVIM_EXE`で使用するNeovim実行ファイルを指定できます。

設定値の型や値が不正な場合、fylerは可能な範囲でその項目だけを無視し、既定値へ
フォールバックします。警告内容はアプリ内のメッセージとして表示されます。

## 設定例

```toml
# 隠しファイルを起動時から表示
show_hidden = false

# ディレクトリを先に並べ、名前の昇順でソート
sort = "dirs_first"
sort_key = "name"
sort_reverse = false

# 保存確認ダイアログに全操作を表示
confirm_detail = "full"

# 日本語fallbackフォントと表示位置の補正
# Windowsパスは、バックスラッシュをそのまま書けるシングルクォートが便利
font = 'C:\Windows\Fonts\meiryo.ttc'
font_y_offset_factor = 0.12

# "ascii"またはNerd Font向けの"nerd"
icons = "ascii"

# Leaderは単一の無修飾キー。省略時はSpace
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

## 設定項目一覧

| 項目 | 型 | 既定値 | 設定値・意味 |
|---|---|---|---|
| `show_hidden` | boolean | `false` | 起動時から隠しファイルを表示する |
| `sort` | string | `"dirs_first"` | `"dirs_first"`または`"mixed"` |
| `sort_key` | string | `"name"` | `"name"`、`"date"`、`"size"`、`"ext"` |
| `sort_reverse` | boolean | `false` | 選択したソートキーを降順にする |
| `confirm_detail` | string | `"full"` | `"full"`または`"summary"` |
| `font` | string | 未指定 | 日本語fallbackフォントの絶対パス |
| `font_y_offset_factor` | number | `0.12` | CJKフォントを下へずらすフォントサイズ比。`0`で無効 |
| `icons` | string | `"ascii"` | `"ascii"`または`"nerd"` |
| `leader` | string | `"Space"` | keymapの`Leader`を展開する単一の無修飾キー |
| `[bookmarks]` | table | 空 | ブックマーク名と絶対パス |
| `[keymap.normal]` | table | 組み込みkeymap | キーシーケンスとaction名 |

### 表示とソート

`show_hidden = true`にすると、ドットで始まる項目とWindowsのhidden属性を持つ項目を
起動時から表示します。起動後は`toggle_hidden` action（既定は`g .`）でも切り替えられます。

`sort`はディレクトリとファイルのグループ方法を指定します。

- `dirs_first`: ディレクトリを先にまとめ、その後へファイルを並べる
- `mixed`: ディレクトリとファイルを混在させて並べる

`sort_key`は各グループ内の比較方法を指定します。

- `name`: 大文字小文字を区別しない自然順。数字部分も数値として比較する
- `date`: 更新日時順
- `size`: ファイルサイズ順
- `ext`: 拡張子順

`sort_reverse = true`はソートキー部分を降順にします。ディレクトリ優先の有無は
`sort`で独立して指定します。起動後は`:sort name|date|size|ext`でも変更でき、
`:sort!`形式では降順になります。

### 確認ダイアログ

`confirm_detail`は保存やtransfer前の確認ダイアログに表示する操作一覧の詳細度です。

- `full`: 操作をすべて表示する
- `summary`: 操作数が多い場合に種類ごとの件数へ要約する

### フォントとアイコン

`font`にはフォントファイルの絶対パスだけを指定できます。相対パスは警告して
無視されます。WindowsパスはTOMLのliteral stringを使うと読みやすくなります。

```toml
font = 'C:\Windows\Fonts\meiryo.ttc'
```

`font_y_offset_factor`は、CJKフォントが上寄りに描画される場合の下方向補正です。
フォントサイズに対する比率で、`0`にすると補正しません。

`icons = "nerd"`はNerd Fontのグリフを使用します。表示用フォントが対応していない
場合は`"ascii"`を使用してください。

### ブックマーク

`[bookmarks]`へ任意の名前と絶対パスを定義します。定義順はブックマーク一覧でも
維持されます。

```toml
[bookmarks]
home = 'C:\Users\me'
work = 'D:\work'
downloads = 'C:\Users\me\Downloads'
```

`:b`で一覧を表示し、`:b home`のように名前を指定して移動できます。一意な前方一致も
利用できます。相対パスや文字列以外の値は警告して無視されます。

最近使ったルートは同じ設定ディレクトリの`recent.toml`へ最大10件保存され、`:b`の
候補へ追加されます。`recent.toml`はfylerが管理するため、通常は手動編集しません。

## Keymap

keymapはエンジン非依存の表記を使用します。Neovim形式の`<C-w>`や`<CR>`は
受け付けません。現在設定できるセクションは`[keymap.normal]`だけです。
`[keymap.visual]`などの未対応セクションは警告して無視されます。

設定は組み込みkeymapへ順に上書きされます。同じキーシーケンスへactionを指定すると
既存の割り当てを置き換え、`"none"`を指定すると割り当てを解除します。

```toml
leader = "Space"

[keymap.normal]
"Leader f" = "file_picker" # Space fへ展開
"g d" = "help"             # 既存キーを別actionへ変更
"g ." = "none"             # 既存キーを解除
```

`activate`、`transfer_move`、`transfer_copy`はNormal modeとVisual modeの両方にmapされます。
その他のactionはNormal mode用です。

### キー表記

複数ストロークは空白で区切り、1ストローク内の修飾キーは`+`で結合します。

```toml
[keymap.normal]
"g d" = "navigate_into"
"Ctrl+W v" = "pane_split_vertical"
"Ctrl+Alt+F5" = "file_picker"
"Leader f" = "file_picker"
```

修飾キー名と名前付きキー名は大文字小文字を区別しません。印字可能文字は区別します。

- 修飾キー: `Ctrl`、`Alt`、`Shift`
- 名前付きキー: `Enter`、`Esc`、`Backspace`、`Tab`、`Delete`、`Space`
- 移動キー: `Up`、`Down`、`Left`、`Right`、`Home`、`End`、`PageUp`、`PageDown`
- ファンクションキー: `F1`～`F12`
- leader参照: `Leader`
- その他の1文字の印字可能文字: `g`、`?`、`V`など

補足と制約:

- `V`は大文字の文字として扱われ、`v`とは別のキーになる
- 修飾付きASCII英字は小文字へ正規化されるため、`Ctrl+W`と`Ctrl+w`は同じ
- 印字可能文字へ`Shift`は指定できない。`Shift+v`ではなく`V`と書く
- `leader`自体は単一の無修飾キーだけ指定できる
- `Leader`を使うbindingで`leader`が不正な場合は、既定の`Space`へフォールバックする
- 単独の`Ctrl+W`には割り当てできない
- `Ctrl+W`で始まるシーケンス同士を、互いに真のprefixとなる形では定義できない
- 不正なキー、未知のaction、重複、解除対象のない`none`は警告の対象になる
- runtime reloadには対応していないため、変更後は再起動が必要

### 設定可能なaction

| action | 説明 | 既定キー |
|---|---|---|
| `activate` | ディレクトリ開閉 / ファイルを開く | `Enter` |
| `navigate_parent` | 親ディレクトリへ移動 | `^` |
| `navigate_into` | ディレクトリ内へ移動 | `g d` |
| `toggle_hidden` | 隠しファイル表示を切り替え | `g .` |
| `fold_close` | ディレクトリを折りたたむ | `z c` |
| `fold_open` | ディレクトリを展開 | `z o` |
| `fold_toggle` | 折りたたみ状態を切り替え | `z a` |
| `fold_close_recursive` | 配下を再帰的に折りたたむ | `z C` |
| `fold_open_recursive` | 配下を再帰的に展開 | `z O` |
| `fold_close_all` | すべて折りたたむ | `z M` |
| `fold_open_all` | すべて展開 | `z R` |
| `file_picker` | ファイルを検索 | `g /` |
| `yank_path` | パスをコピー | `g y` |
| `open_with` | アプリを選んで開く | `g o` |
| `transfer_move` | 別paneへ移動 | `g m` |
| `transfer_copy` | 別paneへコピー | `g c` |
| `help` | ヘルプを表示 | `?` |
| `pane_split_horizontal` | paneを上下分割 | `Ctrl+W s`, `Ctrl+W S` |
| `pane_split_vertical` | paneを左右分割 | `Ctrl+W v` |
| `pane_focus_left` | 左paneへ移動 | `Ctrl+W h` |
| `pane_focus_down` | 下paneへ移動 | `Ctrl+W j` |
| `pane_focus_up` | 上paneへ移動 | `Ctrl+W k` |
| `pane_focus_right` | 右paneへ移動 | `Ctrl+W l` |
| `pane_focus_next` | 次のpaneへ移動 | `Ctrl+W w`, `Ctrl+W Ctrl+W` |
| `pane_focus_previous` | 前のpaneへ移動 | `Ctrl+W p` |
| `pane_close` | paneを閉じる | `Ctrl+W q`, `Ctrl+W c` |

`none`はactionではなく、指定したキーシーケンスの割り当てを解除する特別値です。

`?`で開くヘルプは、組み込みkeymapとユーザー設定を解決した後の割り当てから
動的に生成されます。解除済みactionは表示されず、追加・変更したキーも反映されます。
