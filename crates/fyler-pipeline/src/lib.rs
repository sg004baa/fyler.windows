//! fyler-pipeline — parse / validate / diff(純粋ロジック)。
//!
//! ```text
//! 編集後バッファ → parse → DesiredTree → validate → OperationPlan
//! ```
//!
//! - **実FS・nvim・GUIに一切触れない**。依存はfyler-coreのみ
//! - バッファ文法の解釈は必ず `fyler_core::grammar` を使う(再実装禁止)
//! - Windows名前規則は必ず `fyler_core::win_naming` を使う(再実装禁止)
//! - acceptance criteria: `tests/spec_m2.rs`(実装したら `#[ignore]` を外す)
pub mod diff;
pub mod parse;
pub mod validate;
