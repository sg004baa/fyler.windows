//! fyler-core — 全レイヤーで共有する「型の正典」。
//!
//! 設計の正典は docs/DESIGN.md。このクレートには以下だけを置く:
//!
//! - エンジン非依存のエディタ抽象([`editor`]: `EditorEngine` / `EditorSnapshot` / `EditorCommand`)
//! - in-buffer ID方式のバッファ文法([`grammar`]: 実装済み。再実装禁止)
//! - Windowsの名前規則([`win_naming`]: 実装済み。再実装禁止)
//! - ツリー・差分・実行計画・結果の型([`tree`] / [`plan`] / [`report`] / [`validate`])
//! - 保存処理の状態機械([`save`])
//!
//! 依存境界(AGENTS.md 絶対ルール2): このクレートは std / anyhow / thiserror にしか
//! 依存しない。nvim・egui・Win32の型をここに持ち込まないこと。
pub mod editor;
pub mod grammar;
pub mod id;
pub mod path;
pub mod plan;
pub mod report;
pub mod save;
pub mod tree;
pub mod validate;
pub mod win_naming;

pub use editor::{EditorCommand, EditorEngine, EditorEvent, EditorLine, EditorSnapshot};
pub use id::{EntryId, IdAllocator};
pub use path::TreePath;
pub use plan::{FsOperation, OperationPlan};
pub use report::{CommitReport, OpOutcome, OpResult};
pub use tree::{BaselineEntry, BaselineTree, DesiredEntry, DesiredTree, EditContext, EntryKind};
pub use validate::ValidateError;
