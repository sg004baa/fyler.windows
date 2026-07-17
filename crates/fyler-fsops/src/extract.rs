//! extract: zipアーカイブの展開(右クリックメニュー「Extract Here」系)。
//!
//! 実装契約:
//! - **undo journal 対象外**。展開はアーカイブ本体を変更せず、失敗しても
//!   dest_dir以下を消せばやり直せるため、[`crate::undo`] には記録しない
//! - **非UTF-8エントリ名は全体拒否**する(既知の制限)。SJIS等で名前を格納した
//!   zipは preflight でエラーになる。silent lossy 変換で文字化けした
//!   パスを作るよりも明示的に失敗する方針
//! - 実FS APIへ渡す直前のパスは必ず [`crate::long_path::to_fs`] を通す
//! - preflight は読み取り専用(FS書き込みゼロ)。dest_dir の作成は apply が行い、
//!   dest_dir の作成に失敗した場合は全opを [`OpOutcome::Failed`] として報告する
//!   (zipファイル自体を開けない場合も同様)
//! - このモジュールは cfg 非依存(std + zipクレートのみ)で、非Windowsでも
//!   そのままunit testできる

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow, bail};
use fyler_core::report::{ApplyProgress, CommitReport, OpOutcome, OpResult};
use zip::ZipArchive;

/// zip内の1エントリの展開操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractOp {
    /// zip内の正規化済み相対パス(表示用)。
    pub name: String,
    /// 展開先の絶対パス。
    pub target: PathBuf,
    /// ディレクトリエントリか。
    pub is_dir: bool,
}

/// preflight済みのzip展開計画。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractPlan {
    pub archive: PathBuf,
    pub dest_dir: PathBuf,
    /// zip内エントリ順。
    pub ops: Vec<ExtractOp>,
    /// 宣言された合計展開サイズ(bytes)。
    pub total_bytes: u64,
}

/// zip展開のpreflight。**読み取り専用**でFSへは一切書き込まない。
///
/// エラー条件(いずれも計画全体を拒否する):
/// - `dest_dir` が既に存在する(ファイル・symlinkを含む)
/// - zipを開けない・壊れている
/// - エントリ名がzip-slip(`../`)・絶対パスを含む([`enclosed_name`]がNone)
/// - エントリ名が非UTF-8(生バイトがUTF-8でない、またはUTF-8宣言なしの
///   非ASCII名。cp437へのsilent lossy変換は許さない)
///
/// [`enclosed_name`]: zip::read::ZipFile::enclosed_name
pub fn preflight_extract(archive: &Path, dest_dir: &Path) -> anyhow::Result<ExtractPlan> {
    if fs::symlink_metadata(crate::long_path::to_fs(dest_dir)).is_ok() {
        bail!("Extract destination already exists: {}", dest_dir.display());
    }

    let file = fs::File::open(crate::long_path::to_fs(archive))
        .with_context(|| format!("Failed to open zip archive: {}", archive.display()))?;
    let mut zip = ZipArchive::new(file)
        .with_context(|| format!("Failed to read zip archive: {}", archive.display()))?;

    let mut ops = Vec::with_capacity(zip.len());
    let mut total_bytes: u64 = 0;
    for index in 0..zip.len() {
        let entry = zip
            .by_index_raw(index)
            .with_context(|| format!("Failed to read zip entry #{index}: {}", archive.display()))?;
        // 生バイトがUTF-8であり、かつzipクレートのデコード結果と一致することを
        // 要求する(UTF-8宣言なしの非ASCII名はcp437でデコードされ食い違う)。
        let raw_name = std::str::from_utf8(entry.name_raw()).map_err(|_| {
            anyhow!(
                "Zip entry #{index} has a non-UTF-8 entry name (raw bytes: {:?}): {}",
                entry.name_raw(),
                archive.display()
            )
        })?;
        if entry.name() != raw_name {
            bail!(
                "Zip entry #{index} has a non-UTF-8 entry name (encoding not declared as UTF-8): {}",
                archive.display()
            );
        }
        let relative = entry.enclosed_name().ok_or_else(|| {
            anyhow!("Unsafe zip entry name (path traversal or absolute path): {raw_name}")
        })?;
        total_bytes += entry.size();
        ops.push(ExtractOp {
            name: relative.to_string_lossy().into_owned(),
            target: dest_dir.join(&relative),
            is_dir: entry.is_dir(),
        });
    }

    Ok(ExtractPlan {
        archive: archive.to_path_buf(),
        dest_dir: dest_dir.to_path_buf(),
        ops,
        total_bytes,
    })
}

/// 承認済みの[`ExtractPlan`]を実行する。
///
/// 実装契約([`crate::apply::apply_import_plan_cancellable`]の様式を踏襲):
/// - `plan.ops`を並べ替えず、`CommitReport.results`を同順・同数で返す
/// - zipを開けない・`dest_dir`を作成できない場合は全opを[`OpOutcome::Failed`]にする
/// - キャンセルは操作間だけで反映し、残りを[`OpOutcome::Skipped`]にする
/// - 個別エントリの失敗は[`OpOutcome::Failed`]として記録し、後続を継続する
/// - FS APIの直前には必ず[`crate::long_path::to_fs`]を通す
pub fn apply_extract_cancellable(
    plan: &ExtractPlan,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(ApplyProgress<ExtractOp>),
) -> CommitReport<ExtractOp> {
    let total = plan.ops.len();

    let mut zip = match open_archive(&plan.archive).and_then(|zip| {
        fs::create_dir_all(crate::long_path::to_fs(&plan.dest_dir))
            .with_context(|| {
                format!(
                    "Failed to create extract destination: {}",
                    plan.dest_dir.display()
                )
            })
            .map(|_| zip)
    }) {
        Ok(zip) => zip,
        Err(error) => {
            let error = error.to_string();
            let results = plan
                .ops
                .iter()
                .cloned()
                .map(|op| OpResult {
                    op,
                    outcome: OpOutcome::Failed {
                        error: error.clone(),
                        progress: None,
                    },
                })
                .collect();
            on_progress(ApplyProgress {
                completed: 0,
                total,
                current: None,
            });
            return CommitReport { results };
        }
    };

    let mut results = Vec::with_capacity(total);
    let mut attempted = 0;

    for (index, operation) in plan.ops.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            results.extend(plan.ops[index..].iter().cloned().map(|op| OpResult {
                op,
                outcome: OpOutcome::Skipped {
                    reason: "Cancelled by user".to_owned(),
                },
            }));
            break;
        }

        on_progress(ApplyProgress {
            completed: index,
            total,
            current: Some(operation.clone()),
        });
        attempted += 1;

        let outcome = match execute_extract_operation(&mut zip, index, operation) {
            Ok(()) => OpOutcome::Success,
            Err(error) => OpOutcome::Failed {
                error: error.to_string(),
                progress: None,
            },
        };
        results.push(OpResult {
            op: operation.clone(),
            outcome,
        });
    }

    on_progress(ApplyProgress {
        completed: attempted,
        total,
        current: None,
    });

    CommitReport { results }
}

fn open_archive(archive: &Path) -> anyhow::Result<ZipArchive<fs::File>> {
    let file = fs::File::open(crate::long_path::to_fs(archive))
        .with_context(|| format!("Failed to open zip archive: {}", archive.display()))?;
    ZipArchive::new(file)
        .with_context(|| format!("Failed to read zip archive: {}", archive.display()))
}

/// 1エントリを展開する。opのindexはpreflight時のzipエントリ順と一致する前提で、
/// 実行直前に名前を再検証してアーカイブの外部変更を検出する。
fn execute_extract_operation(
    zip: &mut ZipArchive<fs::File>,
    index: usize,
    operation: &ExtractOp,
) -> anyhow::Result<()> {
    let mut entry = zip
        .by_index(index)
        .with_context(|| format!("Failed to read zip entry: {}", operation.name))?;
    let relative = entry
        .enclosed_name()
        .filter(|relative| relative.to_string_lossy() == operation.name.as_str());
    if relative.is_none() {
        bail!(
            "Zip archive changed since preflight (entry #{index} is no longer {})",
            operation.name
        );
    }

    if operation.is_dir {
        fs::create_dir_all(crate::long_path::to_fs(&operation.target)).with_context(|| {
            format!("Failed to create directory: {}", operation.target.display())
        })?;
        return Ok(());
    }

    if let Some(parent) = operation.target.parent() {
        fs::create_dir_all(crate::long_path::to_fs(parent))
            .with_context(|| format!("Failed to create parent directory: {}", parent.display()))?;
    }
    let mut output = fs::File::create(crate::long_path::to_fs(&operation.target))
        .with_context(|| format!("Failed to create file: {}", operation.target.display()))?;
    std::io::copy(&mut entry, &mut output)
        .with_context(|| format!("Failed to extract: {}", operation.name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    /// `entries`: `(名前, Some(内容)=ファイル / None=ディレクトリ)` の列。
    fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>)]) {
        let file = fs::File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            match data {
                Some(data) => {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(data).unwrap();
                }
                None => writer.add_directory(*name, options).unwrap(),
            }
        }
        writer.finish().unwrap();
    }

    #[test]
    fn preflight_and_apply_extract_nested_zip() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("sample.zip");
        write_zip(
            &archive,
            &[
                ("nested/", None),
                ("nested/inner.txt", Some(b"hello")),
                ("root.txt", Some(b"root")),
            ],
        );
        let dest = dir.path().join("out");

        let plan = preflight_extract(&archive, &dest).unwrap();
        assert_eq!(plan.ops.len(), 3);
        assert_eq!(plan.total_bytes, 9);
        assert!(plan.ops[0].is_dir);
        assert_eq!(plan.ops[1].name, "nested/inner.txt");
        assert_eq!(plan.ops[1].target, dest.join("nested/inner.txt"));

        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();
        let report = apply_extract_cancellable(&plan, &cancel, &mut |p| progress.push(p));

        assert!(report.all_succeeded());
        assert_eq!(report.results.len(), 3);
        assert_eq!(fs::read(dest.join("nested/inner.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dest.join("root.txt")).unwrap(), b"root");
        // 最終通知は completed=総数, current=None。
        let last = progress.last().unwrap();
        assert_eq!(last.completed, 3);
        assert!(last.current.is_none());
    }

    #[test]
    fn preflight_rejects_zip_slip_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("slip.zip");
        write_zip(&archive, &[("../evil.txt", Some(b"evil"))]);

        let error = preflight_extract(&archive, &dir.path().join("out")).unwrap_err();
        assert!(
            error.to_string().contains("Unsafe zip entry name"),
            "{error}"
        );
    }

    #[test]
    fn preflight_rejects_non_utf8_entry_name() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("sjis.zip");
        // ASCII名で書いたzipのバイト列を、同じ長さのSJISバイト列
        // (0x83 0x65 0x83 0x58 = 「テス」)へ差し替えて非UTF-8名を作る。
        write_zip(&archive, &[("AAAA", Some(b"data"))]);
        let mut bytes = fs::read(&archive).unwrap();
        let sjis = [0x83u8, 0x65, 0x83, 0x58];
        let mut patched = 0;
        for start in 0..bytes.len().saturating_sub(4) {
            if &bytes[start..start + 4] == b"AAAA" {
                bytes[start..start + 4].copy_from_slice(&sjis);
                patched += 1;
            }
        }
        assert!(
            patched >= 2,
            "local header + central directory を書き換えたはず"
        );
        fs::write(&archive, bytes).unwrap();

        let error = preflight_extract(&archive, &dir.path().join("out")).unwrap_err();
        assert!(
            error.to_string().contains("non-UTF-8 entry name"),
            "{error}"
        );
    }

    #[test]
    fn preflight_rejects_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("sample.zip");
        write_zip(&archive, &[("a.txt", Some(b"a"))]);
        let dest = dir.path().join("out");
        fs::create_dir(&dest).unwrap();

        let error = preflight_extract(&archive, &dest).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
    }

    #[test]
    fn apply_extract_cancel_skips_remaining_ops() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("sample.zip");
        write_zip(
            &archive,
            &[
                ("a.txt", Some(b"a")),
                ("b.txt", Some(b"b")),
                ("c.txt", Some(b"c")),
            ],
        );
        let dest = dir.path().join("out");
        let plan = preflight_extract(&archive, &dest).unwrap();

        let cancel = AtomicBool::new(false);
        // 最初のopの開始通知でcancelを立てる → 操作間チェックで2つ目以降が止まる。
        let report = apply_extract_cancellable(&plan, &cancel, &mut |p| {
            if p.current.is_some() && p.completed == 0 {
                cancel.store(true, Ordering::Relaxed);
            }
        });

        assert_eq!(report.results.len(), 3);
        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        for result in &report.results[1..] {
            assert!(
                matches!(
                    &result.outcome,
                    OpOutcome::Skipped { reason } if reason == "Cancelled by user"
                ),
                "{:?}",
                result.outcome
            );
        }
        assert!(dest.join("a.txt").exists());
        assert!(!dest.join("b.txt").exists());
    }
}
