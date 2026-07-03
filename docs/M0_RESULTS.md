# M0 成立性スパイク結果

DESIGN.md「M0: 成立性スパイク」の記録。検証コードは `crates/m0-spike`。
**全項目がpassするまでM1以降の実装を始めないこと**(絶対ルール4)。
検証はWindows実機(実IME・実nvim.exe)で行う。

| # | 項目 | 結果 | 検証方法 / メモ |
|---|------|------|------|
| 1 | in-buffer ID: `dd`/`p`, `yy`/`p`, `:m`, `:s`, undo/redo でIDが行に追従する | 未実施 | |
| 2 | in-buffer ID: カーソル列補正と描画隠蔽が破綻しない | 未実施 | |
| 3 | `--embed --headless` 起動 + 後付け `nvim_ui_attach`(最小グリッド) + ext_cmdline / ext_messages のイベント疎通 | 未実施 | |
| 4 | Windows IME(日本語入力)の入力経路(`EditorCommand::Text`) | 未実施 | |
| 5 | 保存状態機械の遷移をコードで確定(`fyler_core::save::transition` のテストを通す) | 未実施 | |
| 6 | バッファ文法の最終確定(`fyler_core::grammar` のテストが仕様通りであることの確認) | 未実施 | |

## 判定

- [ ] M0 完了(全項目 pass) — 完了したら AGENTS.md のマイルストーン現況も更新する

## 失敗時の撤退ルート(DESIGN.mdより)

- カーソル補正が破綻した場合: IDプレフィックスを固定幅にする、
  またはカーソル移動をフックして補正するfallbackを検討する
