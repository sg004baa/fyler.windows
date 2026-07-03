# M0 成立性スパイク結果

DESIGN.md「M0: 成立性スパイク」の記録。検証コードは `crates/m0-spike`。
**全項目がpassするまでM1以降の実装を始めないこと**(絶対ルール4)。
検証はWindows実機(実IME・実nvim.exe)で行う。

| # | 項目 | 結果 | 検証方法 / メモ |
|---|------|------|------|
| 1 | in-buffer ID: `dd`/`p`, `yy`/`p`, `:m`, `:s`, undo/redo でIDが行に追従する | pass | 実nvim(v0.11.6)へRPCで `nvim_buf_set_lines` 投入 → `nvim_input` で各操作 → `nvim_buf_get_lines` 検証。`dd`→`p`: `/013 ` が内容ごと移動(孤児化・欠落なし)。`yy`→`p`: 同一 `/013 ` が2行に複製=COPYシグナル。`:1m$`: 移動後も各 `/NNN ` 保持。`:2s/main/MAIN/`: 名前部のみ変更・ID保持。`u`/`<C-r>`: 元に戻る/やり直せる。全行 `grammar::split_id_prefix` が Broken 無し。プレフィックス剥がれ・別行混入なし |
| 2 | in-buffer ID: カーソル列補正と描画隠蔽が破綻しない | pass | `/012 新規ファイル.txt`(`id_prefix_len`=5)で `nvim_win_set_cursor`/`l`移動→`nvim_win_get_cursor` の実バイト列を収集。表示列=`col-id_prefix_len`(col<5は0にクランプ)。観測列 0..5→disp0(prefix領域クランプ成立)、名前部は 5,8,11,14,17,20,23(3バイト刻み)で全て UTF-8境界(`is_char_boundary`=true)・ズレなし。日本語名で破綻せず |
| 3 | `--embed --headless` 起動 + 後付け `nvim_ui_attach`(最小グリッド) + ext_cmdline / ext_messages のイベント疎通 | pass | `NVIM_ARGS` で spawn(Windowsで `CREATE_NO_WINDOW` 付与)。(a) `nvim_get_api_info` 往復OK(channel=1)。(b) 起動後に `nvim_ui_attach(80x24, ext_cmdline+ext_messages のみ, 他false)` 成功(ext_linegrid不要)、`:` 入力で `cmdline_show` 受信。(c) `/存在しない語<CR>` で `msg_show`(E486: Pattern not found)受信。grid系イベントは無視。(d) `--headless`+`CREATE_NO_WINDOW` によりコンソールウィンドウは構造上生成されない(フラグ適用・spawn成功で確認) |
| 4 | Windows IME(日本語入力)の入力経路(`EditorCommand::Text`) | pass | 確定文字列 `新規ファイル` を (A)`nvim_input` 直渡し と (B)`nvim_paste` / `nvim_buf_set_text` で挿入比較。両経路とも `新規ファイル` がリテラルで入る。keycode混入文字列 `foo<CR>bar` では (A)input が `["foo","bar"]` に化ける一方、(B)paste は `foo<CR>bar` をリテラル保持。**採用: `EditorCommand::Text` は `nvim_paste`(カーソル位置へリテラル挿入)、代替 `nvim_buf_set_text`(モード非依存)。`nvim_input` は不採用**(engine.rs 実装契約6の確定材料) |
| 5 | 保存状態機械の遷移をコードで確定(`fyler_core::save::transition` のテストを通す) | pass | Linux上で単体テスト3件 pass |
| 6 | バッファ文法の最終確定(`fyler_core::grammar` のテストが仕様通りであることの確認) | pass | Linux上で単体テスト7件 pass。DESIGN.md「行フォーマット」「バッファ文法の決定事項」(prefix `/{id} `・半角2=1階層・IDなし=CREATE・Broken=保存中断・末尾`/`・3桁ゼロ埋め)と全項目一致 |

## 判定

- [x] M0 完了(全項目 pass) — 完了したら AGENTS.md のマイルストーン現況も更新する

### 検証環境 / 補足

- 実機: Windows 11 (win32 10.0.26200) / Neovim v0.11.6 / nvim-rs `=0.9.2`(features=`use_tokio`)。
- 実行: `FYLER_NVIM_EXE="C:\Program Files\Neovim\bin\nvim.exe" cargo run -p m0-spike` で #1..#4 が ALL PASS。
- 検証コード: `crates/m0-spike/src/main.rs`(spike専用。製品コードには含めない)。
  spikeは境界ルールの例外として直接 nvim-rs を使用(製品クレートの `todo!()` スタブは未変更)。
- (任意) `KeyInput → keycode` マッピング(`'a'`→`a`, `'<'`→`<lt>`, Enter→`<CR>`, Esc→`<Esc>`,
  修飾は `<C-A-S->` 順 等)を机上検証16件 pass。製品 `translate.rs` の実装はM1で行う。
- 実FSへの破壊的操作は一切なし(#1のID追従はnvimバッファ行の読み書きのみで検証)。

## 失敗時の撤退ルート(DESIGN.mdより)

- カーソル補正が破綻した場合: IDプレフィックスを固定幅にする、
  またはカーソル移動をフックして補正するfallbackを検討する
