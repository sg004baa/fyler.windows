//! fyler — エントリポイント。各レイヤーの配線だけを行う(ロジックを書かない)。
//!
//! 各レイヤーの役割はAGENTS.mdの依存境界表を参照。ここに書いてよいのは
//! 「起動」「イベントの受け渡し」「保存状態機械の副作用(SaveEffect)の実行」のみ。

fn main() -> anyhow::Result<()> {
    // M1で実装する起動フロー:
    //
    // 1. 引数から表示ルートディレクトリを取得(省略時はカレントディレクトリ)
    // 2. tokioランタイムを起動し、NvimEngine::start(NvimConfig)でエンジン開始
    //    (GUIはメインスレッド必須のため、tokioはバックグラウンドランタイムにする)
    // 3. fyler_fsops::scan::scan_baseline でBaselineTreeを構築し、
    //    fyler_core::grammar::format_id_prefix で初期バッファ内容を生成して
    //    エンジンに流し込む
    // 4. fyler_gui::app::run(engine) でGUI起動(メインスレッドをeframeに渡す)
    //
    // M2で追加する配線:
    // - EditorEvent::CommitRequested → fyler_core::save::transition →
    //   SaveEffectの実行(RunPipeline = fyler_pipeline::{parse,validate,diff}、
    //   ExecutePlan = fyler_fsops::apply::apply_plan ※M3から。M2はdry-runのみ)
    //
    // M5で追加する配線:
    // - fyler_fsops::watch → 再スキャン・再描画(dirty中は通知のみ)
    // - アプリmanifestに longPathAware を入れる(ビルド設定)
    todo!("M1: 起動配線")
}
