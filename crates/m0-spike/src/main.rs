//! M0: 成立性スパイク(DESIGN.md「マイルストーン」)。
//!
//! **絶対ルール4: ここが全項目passするまでM1以降の実装を始めない。**
//! 各項目の検証コードをこのクレートに実装し(検証のためであれば汚くてよい。
//! ここのコードは製品コードに含めない)、結果を docs/M0_RESULTS.md に記録する。
//!
//! 検証はWindows実機で行う(IME・nvim.exe・コンソール挙動が対象のため)。
//! nvim RPCの実験には fyler-engine-nvim ではなく、このクレート内で直接
//! nvim-rsを使ってよい(スパイクは境界ルールの例外。製品コードは例外なし)。

const CHECKLIST: &[&str] = &[
    "in-buffer ID方式: dd/p, yy/p, :m, :s, undo/redo でIDが行に追従する",
    "in-buffer ID方式: カーソル列補正と描画隠蔽が破綻しない(プレフィックス領域へのカーソル進入の補正方法を確定)",
    "nvim --embed --headless 起動 + 後付けnvim_ui_attach(最小グリッド) + ext_cmdline/ext_messages のイベント疎通",
    "Windows IME(日本語入力)の入力経路確認(EditorCommand::Text で確定文字列を流す方式が成立するか)",
    "保存状態機械の遷移をコードで確定(fyler_core::save::transition を実装し #[ignore] テストを通す)",
    "バッファ文法の最終確定(fyler_core::grammar の実装・テストが決定事項どおりか確認)",
];

fn main() {
    println!("=== M0 成立性スパイク チェックリスト ===\n");
    for (i, item) in CHECKLIST.iter().enumerate() {
        println!("  [ ] {}. {item}", i + 1);
    }
    println!(
        "\n各項目の検証コードをこのクレートに実装し、結果を docs/M0_RESULTS.md に記録すること。"
    );
    println!("全項目passするまでM1以降の実装を始めないこと(AGENTS.md 絶対ルール4)。");
}
