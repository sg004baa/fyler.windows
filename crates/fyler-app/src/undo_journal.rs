//! undo transactionの永続journal。
//!
//! journalはfyler自身の状態ディレクトリ配下だけを書き換える。表示中ツリーへの
//! 実FS操作は行わず、undo実行そのものは`fyler-fsops`のworkerに委ねる。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use fyler_core::tree::EntryKind;
use fyler_core::undo::{
    BackupRef, FileIdentity, Fingerprint, ManifestEntry, UndoStep, UndoTransaction,
};

const MANIFEST_FILE: &str = "manifest.toml";
const PAYLOAD_DIR: &str = "payload";

static TX_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// undo journal全体の保存先。
#[derive(Debug, Clone)]
pub struct UndoJournal {
    dir: PathBuf,
}

/// 起動時復旧の対象になるtransactionディレクトリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub id: String,
    pub state: JournalState,
    pub dir: PathBuf,
}

/// WAL manifestに記録するtransaction状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    Preparing,
    Committed,
    Undoing,
    Undone,
}

impl UndoJournal {
    /// undo journalを開く。存在しない場合は状態ディレクトリを作成する。
    pub fn open() -> anyhow::Result<Self> {
        let dir = undo_dir()?;
        fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create undo journal directory: {}", dir.display())
        })?;
        Ok(Self { dir })
    }

    /// apply承認直後にtransactionディレクトリとPreparing manifestを作成する。
    pub fn begin(&self, id: &str, root: &Path) -> anyhow::Result<PathBuf> {
        let root = path_string(root).with_context(|| {
            format!(
                "Root recorded in undo journal is not UTF-8: {}",
                root.display()
            )
        })?;
        let transaction_dir = self.transaction_dir(id);
        fs::create_dir_all(&transaction_dir).with_context(|| {
            format!(
                "Failed to create undo transaction directory: {}",
                transaction_dir.display()
            )
        })?;

        let mut table = toml::Table::new();
        table.insert("id".to_owned(), toml::Value::String(id.to_owned()));
        table.insert(
            "state".to_owned(),
            toml::Value::String(JournalState::Preparing.as_str().to_owned()),
        );
        table.insert("root".to_owned(), toml::Value::String(root));
        table.insert("backup_dir".to_owned(), toml::Value::Boolean(false));
        table.insert("steps".to_owned(), toml::Value::Array(Vec::new()));
        write_manifest(&transaction_dir, table)?;
        Ok(transaction_dir)
    }

    /// apply完了後にtransaction全体をCommitted manifestとして保存する。
    pub fn commit(&self, transaction: &UndoTransaction) -> anyhow::Result<()> {
        let transaction_dir = self.transaction_dir(&transaction.id);
        fs::create_dir_all(&transaction_dir).with_context(|| {
            format!(
                "Failed to create undo transaction directory: {}",
                transaction_dir.display()
            )
        })?;
        let Some(table) = transaction_manifest_table(transaction, JournalState::Committed)? else {
            return Ok(());
        };
        write_manifest(&transaction_dir, table)
    }

    /// undo承認直後にmanifestをUndoingへ進める。
    pub fn mark_undoing(&self, id: &str) -> anyhow::Result<()> {
        self.update_state(id, JournalState::Undoing)
    }

    /// undo完了後にpayloadを破棄し、transactionディレクトリを削除する。
    pub fn finish_undone(&self, id: &str) -> anyhow::Result<()> {
        self.update_state(id, JournalState::Undone)?;
        let transaction_dir = self.transaction_dir(id);
        let payload = transaction_dir.join(PAYLOAD_DIR);
        if let Err(error) = fs::remove_dir_all(&payload)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error)
                .with_context(|| format!("Failed to remove undo payload: {}", payload.display()));
        }
        remove_dir_all_if_exists(&transaction_dir)
    }

    /// slot破棄時にtransactionディレクトリを丸ごと削除する。
    pub fn discard(&self, id: &str) -> anyhow::Result<()> {
        remove_dir_all_if_exists(&self.transaction_dir(id))
    }

    /// 起動時にjournalを走査し、手動判断が必要なPreparing/Undoingだけを返す。
    pub fn scan_on_startup(&self) -> anyhow::Result<Vec<JournalEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)
            .with_context(|| format!("Failed to scan undo journal: {}", self.dir.display()))?
        {
            let entry = entry?;
            let transaction_dir = entry.path();
            if !transaction_dir.is_dir() {
                continue;
            }
            let fallback_id = entry.file_name().to_string_lossy().into_owned();
            let state = match read_manifest_state(&transaction_dir) {
                Ok((id, state)) => match state {
                    JournalState::Preparing | JournalState::Undoing => {
                        entries.push(JournalEntry {
                            id,
                            state,
                            dir: transaction_dir,
                        });
                        continue;
                    }
                    JournalState::Committed | JournalState::Undone => state,
                },
                Err(error) => {
                    eprintln!(
                        "Failed to read undo journal manifest; retaining it as a recovery candidate: {}: {error:#}",
                        transaction_dir.display()
                    );
                    entries.push(JournalEntry {
                        id: fallback_id,
                        state: JournalState::Preparing,
                        dir: transaction_dir,
                    });
                    continue;
                }
            };
            if matches!(state, JournalState::Committed | JournalState::Undone) {
                remove_dir_all_if_exists(&transaction_dir)?;
            }
        }
        Ok(entries)
    }

    fn transaction_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    fn update_state(&self, id: &str, state: JournalState) -> anyhow::Result<()> {
        let transaction_dir = self.transaction_dir(id);
        let source =
            fs::read_to_string(transaction_dir.join(MANIFEST_FILE)).with_context(|| {
                format!(
                    "Failed to read undo manifest: {}",
                    transaction_dir.join(MANIFEST_FILE).display()
                )
            })?;
        let mut table = source
            .parse::<toml::Table>()
            .context("Undo manifest contains invalid TOML")?;
        table.insert(
            "state".to_owned(),
            toml::Value::String(state.as_str().to_owned()),
        );
        write_manifest(&transaction_dir, table)
    }
}

/// 現プロセス内で一意なundo transaction IDを生成する。
pub fn new_transaction_id() -> String {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = TX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{unix_ms}-{}-{sequence}", std::process::id())
}

impl JournalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "Preparing",
            Self::Committed => "Committed",
            Self::Undoing => "Undoing",
            Self::Undone => "Undone",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "Preparing" => Ok(Self::Preparing),
            "Committed" => Ok(Self::Committed),
            "Undoing" => Ok(Self::Undoing),
            "Undone" => Ok(Self::Undone),
            _ => anyhow::bail!("Unknown undo journal state: {value}"),
        }
    }
}

fn undo_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = nonempty_env("FYLER_UNDO_DIR") {
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        nonempty_env("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("fyler").join("undo"))
            .context("LOCALAPPDATA is not set")
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = nonempty_env("XDG_STATE_HOME") {
            return Ok(PathBuf::from(path).join("fyler").join("undo"));
        }
        nonempty_env("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".local").join("state").join("fyler").join("undo"))
            .context("Neither XDG_STATE_HOME nor HOME is set")
    }
}

fn nonempty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn write_manifest(transaction_dir: &Path, table: toml::Table) -> anyhow::Result<()> {
    fs::create_dir_all(transaction_dir).with_context(|| {
        format!(
            "Failed to create undo transaction directory: {}",
            transaction_dir.display()
        )
    })?;
    let target = transaction_dir.join(MANIFEST_FILE);
    let temporary = transaction_dir.join(format!(".{MANIFEST_FILE}.{}.tmp", std::process::id()));
    fs::write(&temporary, table.to_string()).with_context(|| {
        format!(
            "Failed to write temporary undo manifest: {}",
            temporary.display()
        )
    })?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("Failed to replace undo manifest: {}", target.display()));
    }
    Ok(())
}

fn remove_dir_all_if_exists(path: &Path) -> anyhow::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to remove directory: {}", path.display()))
        }
    }
}

fn transaction_manifest_table(
    transaction: &UndoTransaction,
    state: JournalState,
) -> anyhow::Result<Option<toml::Table>> {
    let Some(root) = path_string_opt(&transaction.root) else {
        return Ok(None);
    };
    let mut steps = Vec::with_capacity(transaction.steps.len());
    for step in &transaction.steps {
        let Some(table) = step_table(step)? else {
            return Ok(None);
        };
        steps.push(toml::Value::Table(table));
    }

    let mut table = toml::Table::new();
    table.insert("id".to_owned(), toml::Value::String(transaction.id.clone()));
    table.insert(
        "state".to_owned(),
        toml::Value::String(state.as_str().to_owned()),
    );
    table.insert("root".to_owned(), toml::Value::String(root));
    table.insert(
        "backup_dir".to_owned(),
        toml::Value::Boolean(transaction.backup_dir.is_some()),
    );
    table.insert("steps".to_owned(), toml::Value::Array(steps));
    Ok(Some(table))
}

fn step_table(step: &UndoStep) -> anyhow::Result<Option<toml::Table>> {
    let mut table = toml::Table::new();
    match step {
        UndoStep::RemoveCreated {
            path,
            identity,
            post,
        } => {
            let Some(path) = path_string_opt(path) else {
                return Ok(None);
            };
            let Some(post) = fingerprint_table(post)? else {
                return Ok(None);
            };
            table.insert(
                "type".to_owned(),
                toml::Value::String("remove_created".to_owned()),
            );
            table.insert("path".to_owned(), toml::Value::String(path));
            insert_identity(&mut table, identity);
            table.insert("post".to_owned(), toml::Value::Table(post));
        }
        UndoStep::RemoveCopied {
            path,
            identity,
            post,
            manifest,
        } => {
            let Some(path) = path_string_opt(path) else {
                return Ok(None);
            };
            let Some(post) = fingerprint_table(post)? else {
                return Ok(None);
            };
            table.insert(
                "type".to_owned(),
                toml::Value::String("remove_copied".to_owned()),
            );
            table.insert("path".to_owned(), toml::Value::String(path));
            insert_identity(&mut table, identity);
            table.insert("post".to_owned(), toml::Value::Table(post));
            if let Some(manifest) = manifest {
                let mut entries = Vec::with_capacity(manifest.len());
                for entry in manifest {
                    entries.push(toml::Value::Table(manifest_entry_table(entry)?));
                }
                table.insert("manifest".to_owned(), toml::Value::Array(entries));
            }
        }
        UndoStep::MoveBack {
            from,
            to,
            identity,
            post,
            case_only,
        } => {
            let (Some(from), Some(to)) = (path_string_opt(from), path_string_opt(to)) else {
                return Ok(None);
            };
            let Some(post) = fingerprint_table(post)? else {
                return Ok(None);
            };
            table.insert(
                "type".to_owned(),
                toml::Value::String("move_back".to_owned()),
            );
            table.insert("from".to_owned(), toml::Value::String(from));
            table.insert("to".to_owned(), toml::Value::String(to));
            insert_identity(&mut table, identity);
            table.insert("post".to_owned(), toml::Value::Table(post));
            table.insert("case_only".to_owned(), toml::Value::Boolean(*case_only));
        }
        UndoStep::RestoreDeleted { path, backup } => {
            let Some(path) = path_string_opt(path) else {
                return Ok(None);
            };
            table.insert(
                "type".to_owned(),
                toml::Value::String("restore_deleted".to_owned()),
            );
            table.insert("path".to_owned(), toml::Value::String(path));
            table.insert(
                "backup".to_owned(),
                toml::Value::Table(backup_table(backup)),
            );
        }
        UndoStep::RestoreOverwritten { path, backup } => {
            let Some(path) = path_string_opt(path) else {
                return Ok(None);
            };
            table.insert(
                "type".to_owned(),
                toml::Value::String("restore_overwritten".to_owned()),
            );
            table.insert("path".to_owned(), toml::Value::String(path));
            table.insert(
                "backup".to_owned(),
                toml::Value::Table(backup_table(backup)),
            );
        }
    }
    Ok(Some(table))
}

fn insert_identity(table: &mut toml::Table, identity: &Option<FileIdentity>) {
    if let Some(identity) = identity {
        let mut identity_table = toml::Table::new();
        identity_table.insert(
            "volume".to_owned(),
            toml::Value::String(identity.volume.to_string()),
        );
        identity_table.insert(
            "file".to_owned(),
            toml::Value::String(identity.file.to_string()),
        );
        table.insert("identity".to_owned(), toml::Value::Table(identity_table));
    }
}

fn fingerprint_table(fingerprint: &Fingerprint) -> anyhow::Result<Option<toml::Table>> {
    let mut table = toml::Table::new();
    table.insert(
        "kind".to_owned(),
        toml::Value::String(kind_to_str(fingerprint.kind).to_owned()),
    );
    if let Some(size) = fingerprint.size {
        table.insert("size".to_owned(), u64_value(size)?);
    }
    insert_time(&mut table, "mtime", fingerprint.mtime)?;
    if let Some(link_target) = &fingerprint.link_target {
        let Some(link_target) = path_string_opt(link_target) else {
            return Ok(None);
        };
        table.insert("link_target".to_owned(), toml::Value::String(link_target));
    }
    Ok(Some(table))
}

fn manifest_entry_table(entry: &ManifestEntry) -> anyhow::Result<toml::Table> {
    let mut table = toml::Table::new();
    table.insert(
        "rel_path".to_owned(),
        toml::Value::String(entry.rel_path.clone()),
    );
    table.insert(
        "kind".to_owned(),
        toml::Value::String(kind_to_str(entry.kind).to_owned()),
    );
    if let Some(size) = entry.size {
        table.insert("size".to_owned(), u64_value(size)?);
    }
    insert_time(&mut table, "mtime", entry.mtime)?;
    Ok(table)
}

fn backup_table(backup: &BackupRef) -> toml::Table {
    let mut table = toml::Table::new();
    table.insert(
        "payload_rel".to_owned(),
        toml::Value::String(backup.payload_rel.clone()),
    );
    table.insert(
        "kind".to_owned(),
        toml::Value::String(kind_to_str(backup.kind).to_owned()),
    );
    table
}

fn insert_time(
    table: &mut toml::Table,
    prefix: &str,
    time: Option<SystemTime>,
) -> anyhow::Result<()> {
    let Some(time) = time else {
        return Ok(());
    };
    let duration = time
        .duration_since(UNIX_EPOCH)
        .context("mtime before UNIX_EPOCH cannot be recorded in the journal")?;
    table.insert(
        format!("{prefix}_sec"),
        toml::Value::Integer(i64::try_from(duration.as_secs())?),
    );
    table.insert(
        format!("{prefix}_nanos"),
        toml::Value::Integer(i64::from(duration.subsec_nanos())),
    );
    Ok(())
}

fn u64_value(value: u64) -> anyhow::Result<toml::Value> {
    Ok(toml::Value::Integer(i64::try_from(value)?))
}

fn read_manifest_state(transaction_dir: &Path) -> anyhow::Result<(String, JournalState)> {
    let table = read_manifest_table(transaction_dir)?;
    let id = string_field(&table, "id")?.to_owned();
    let state = JournalState::from_str(string_field(&table, "state")?)?;
    Ok((id, state))
}

#[cfg(test)]
fn read_transaction(transaction_dir: &Path) -> anyhow::Result<(JournalState, UndoTransaction)> {
    let table = read_manifest_table(transaction_dir)?;
    let id = string_field(&table, "id")?.to_owned();
    let state = JournalState::from_str(string_field(&table, "state")?)?;
    let root = PathBuf::from(string_field(&table, "root")?);
    let backup_dir = bool_field(&table, "backup_dir")?.then(|| transaction_dir.to_path_buf());
    let steps = array_field(&table, "steps")?
        .iter()
        .map(parse_step)
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((
        state,
        UndoTransaction {
            id,
            root,
            steps,
            backup_dir,
        },
    ))
}

fn read_manifest_table(transaction_dir: &Path) -> anyhow::Result<toml::Table> {
    let path = transaction_dir.join(MANIFEST_FILE);
    fs::read_to_string(&path)
        .with_context(|| format!("Failed to read undo manifest: {}", path.display()))?
        .parse::<toml::Table>()
        .context("Undo manifest contains invalid TOML")
}

#[cfg(test)]
fn parse_step(value: &toml::Value) -> anyhow::Result<UndoStep> {
    let table = value.as_table().context("Undo step must be a table")?;
    match string_field(table, "type")? {
        "remove_created" => Ok(UndoStep::RemoveCreated {
            path: PathBuf::from(string_field(table, "path")?),
            identity: parse_identity(table)?,
            post: parse_fingerprint(table_field(table, "post")?)?,
        }),
        "remove_copied" => Ok(UndoStep::RemoveCopied {
            path: PathBuf::from(string_field(table, "path")?),
            identity: parse_identity(table)?,
            post: parse_fingerprint(table_field(table, "post")?)?,
            manifest: optional_array_field(table, "manifest")?
                .map(|entries| {
                    entries
                        .iter()
                        .map(parse_manifest_entry)
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .transpose()?,
        }),
        "move_back" => Ok(UndoStep::MoveBack {
            from: PathBuf::from(string_field(table, "from")?),
            to: PathBuf::from(string_field(table, "to")?),
            identity: parse_identity(table)?,
            post: parse_fingerprint(table_field(table, "post")?)?,
            case_only: bool_field(table, "case_only")?,
        }),
        "restore_deleted" => Ok(UndoStep::RestoreDeleted {
            path: PathBuf::from(string_field(table, "path")?),
            backup: parse_backup(table_field(table, "backup")?)?,
        }),
        "restore_overwritten" => Ok(UndoStep::RestoreOverwritten {
            path: PathBuf::from(string_field(table, "path")?),
            backup: parse_backup(table_field(table, "backup")?)?,
        }),
        kind => anyhow::bail!("Unknown undo step type: {kind}"),
    }
}

#[cfg(test)]
fn parse_identity(table: &toml::Table) -> anyhow::Result<Option<FileIdentity>> {
    let Some(value) = table.get("identity") else {
        return Ok(None);
    };
    let table = value.as_table().context("identity must be a table")?;
    Ok(Some(FileIdentity {
        volume: string_field(table, "volume")?.parse()?,
        file: string_field(table, "file")?.parse()?,
    }))
}

#[cfg(test)]
fn parse_fingerprint(table: &toml::Table) -> anyhow::Result<Fingerprint> {
    Ok(Fingerprint {
        kind: parse_kind(string_field(table, "kind")?)?,
        size: optional_u64_field(table, "size")?,
        mtime: parse_time(table, "mtime")?,
        link_target: optional_string_field(table, "link_target")?.map(PathBuf::from),
    })
}

#[cfg(test)]
fn parse_manifest_entry(value: &toml::Value) -> anyhow::Result<ManifestEntry> {
    let table = value.as_table().context("manifest entry must be a table")?;
    Ok(ManifestEntry {
        rel_path: string_field(table, "rel_path")?.to_owned(),
        kind: parse_kind(string_field(table, "kind")?)?,
        size: optional_u64_field(table, "size")?,
        mtime: parse_time(table, "mtime")?,
    })
}

#[cfg(test)]
fn parse_backup(table: &toml::Table) -> anyhow::Result<BackupRef> {
    Ok(BackupRef {
        payload_rel: string_field(table, "payload_rel")?.to_owned(),
        kind: parse_kind(string_field(table, "kind")?)?,
    })
}

#[cfg(test)]
fn parse_time(table: &toml::Table, prefix: &str) -> anyhow::Result<Option<SystemTime>> {
    let Some(sec) = optional_i64_field(table, &format!("{prefix}_sec"))? else {
        return Ok(None);
    };
    let nanos = optional_i64_field(table, &format!("{prefix}_nanos"))?.unwrap_or(0);
    let secs = u64::try_from(sec).context("Negative mtime seconds are not supported")?;
    let nanos = u32::try_from(nanos).context("mtime nanos is outside the u32 range")?;
    Ok(Some(UNIX_EPOCH + std::time::Duration::new(secs, nanos)))
}

fn string_field<'a>(table: &'a toml::Table, key: &str) -> anyhow::Result<&'a str> {
    table
        .get(key)
        .and_then(toml::Value::as_str)
        .with_context(|| format!("{key} must be a string"))
}

#[cfg(test)]
fn optional_string_field<'a>(table: &'a toml::Table, key: &str) -> anyhow::Result<Option<&'a str>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{key} must be a string"))
        })
        .transpose()
}

#[cfg(test)]
fn table_field<'a>(table: &'a toml::Table, key: &str) -> anyhow::Result<&'a toml::Table> {
    table
        .get(key)
        .and_then(toml::Value::as_table)
        .with_context(|| format!("{key} must be a table"))
}

#[cfg(test)]
fn array_field<'a>(table: &'a toml::Table, key: &str) -> anyhow::Result<&'a Vec<toml::Value>> {
    table
        .get(key)
        .and_then(toml::Value::as_array)
        .with_context(|| format!("{key} must be an array"))
}

#[cfg(test)]
fn optional_array_field<'a>(
    table: &'a toml::Table,
    key: &str,
) -> anyhow::Result<Option<&'a Vec<toml::Value>>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_array()
                .with_context(|| format!("{key} must be an array"))
        })
        .transpose()
}

#[cfg(test)]
fn bool_field(table: &toml::Table, key: &str) -> anyhow::Result<bool> {
    table
        .get(key)
        .and_then(toml::Value::as_bool)
        .with_context(|| format!("{key} must be a boolean"))
}

#[cfg(test)]
fn optional_i64_field(table: &toml::Table, key: &str) -> anyhow::Result<Option<i64>> {
    table
        .get(key)
        .map(|value| {
            value
                .as_integer()
                .with_context(|| format!("{key} must be an integer"))
        })
        .transpose()
}

#[cfg(test)]
fn optional_u64_field(table: &toml::Table, key: &str) -> anyhow::Result<Option<u64>> {
    optional_i64_field(table, key)?
        .map(|value| u64::try_from(value).with_context(|| format!("{key} is negative")))
        .transpose()
}

fn kind_to_str(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
    }
}

#[cfg(test)]
fn parse_kind(value: &str) -> anyhow::Result<EntryKind> {
    match value {
        "file" => Ok(EntryKind::File),
        "dir" => Ok(EntryKind::Dir),
        "symlink" => Ok(EntryKind::Symlink),
        _ => anyhow::bail!("Unknown entry kind: {value}"),
    }
}

fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_owned)
        .context("Path is not UTF-8")
}

fn path_string_opt(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    impl UndoJournal {
        fn at_dir(dir: PathBuf) -> Self {
            Self { dir }
        }
    }

    struct UndoDirEnv {
        previous: Option<OsString>,
    }

    impl UndoDirEnv {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("FYLER_UNDO_DIR");
            // SAFETY: FYLER_UNDO_DIRを変更するテストはこの1件だけで完結する。
            unsafe {
                std::env::set_var("FYLER_UNDO_DIR", path);
            }
            Self { previous }
        }
    }

    impl Drop for UndoDirEnv {
        fn drop(&mut self) {
            // SAFETY: FYLER_UNDO_DIRを変更するテストはこの1件だけで完結する。
            unsafe {
                match self.previous.take() {
                    Some(previous) => std::env::set_var("FYLER_UNDO_DIR", previous),
                    None => std::env::remove_var("FYLER_UNDO_DIR"),
                }
            }
        }
    }

    fn test_time(offset: u64) -> SystemTime {
        UNIX_EPOCH + Duration::new(1_700_000_000 + offset, 123_000_000)
    }

    fn fingerprint(kind: EntryKind) -> Fingerprint {
        Fingerprint {
            kind,
            size: (kind == EntryKind::File).then_some(42),
            mtime: (kind == EntryKind::File).then_some(test_time(1)),
            link_target: (kind == EntryKind::Symlink).then(|| PathBuf::from("/target")),
        }
    }

    fn backup(kind: EntryKind) -> BackupRef {
        BackupRef {
            payload_rel: "payload/0/deleted.txt".to_owned(),
            kind,
        }
    }

    fn all_step_transaction(id: &str, dir: &Path, root: &Path) -> UndoTransaction {
        UndoTransaction {
            id: id.to_owned(),
            root: root.to_path_buf(),
            steps: vec![
                UndoStep::RemoveCreated {
                    path: root.join("created.txt"),
                    identity: Some(FileIdentity { volume: 1, file: 2 }),
                    post: fingerprint(EntryKind::File),
                },
                UndoStep::RemoveCopied {
                    path: root.join("copied-dir"),
                    identity: Some(FileIdentity { volume: 3, file: 4 }),
                    post: fingerprint(EntryKind::Dir),
                    manifest: Some(vec![ManifestEntry {
                        rel_path: "child.txt".to_owned(),
                        kind: EntryKind::File,
                        size: Some(7),
                        mtime: Some(test_time(2)),
                    }]),
                },
                UndoStep::MoveBack {
                    from: root.join("old.txt"),
                    to: root.join("new.txt"),
                    identity: None,
                    post: fingerprint(EntryKind::File),
                    case_only: false,
                },
                UndoStep::RestoreDeleted {
                    path: root.join("deleted.txt"),
                    backup: backup(EntryKind::File),
                },
                UndoStep::RestoreOverwritten {
                    path: root.join("overwritten.txt"),
                    backup: BackupRef {
                        payload_rel: "payload/4/overwritten.txt".to_owned(),
                        kind: EntryKind::File,
                    },
                },
            ],
            backup_dir: Some(dir.to_path_buf()),
        }
    }

    #[test]
    fn wal_lifecycle_writes_states_and_finish_removes_transaction_dir() {
        let undo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let journal = UndoJournal::at_dir(undo.path().to_path_buf());

        let transaction_dir = journal.begin("tx-1", root.path()).unwrap();
        assert_eq!(
            read_manifest_state(&transaction_dir).unwrap(),
            ("tx-1".to_owned(), JournalState::Preparing)
        );

        let transaction = all_step_transaction("tx-1", &transaction_dir, root.path());
        journal.commit(&transaction).unwrap();
        assert_eq!(
            read_manifest_state(&transaction_dir).unwrap(),
            ("tx-1".to_owned(), JournalState::Committed)
        );

        journal.mark_undoing("tx-1").unwrap();
        assert_eq!(
            read_manifest_state(&transaction_dir).unwrap(),
            ("tx-1".to_owned(), JournalState::Undoing)
        );

        fs::create_dir_all(transaction_dir.join(PAYLOAD_DIR)).unwrap();
        journal.finish_undone("tx-1").unwrap();
        assert!(!transaction_dir.exists());
    }

    #[test]
    fn discard_removes_transaction_directory() {
        let undo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let journal = UndoJournal::at_dir(undo.path().to_path_buf());
        let transaction_dir = journal.begin("tx-discard", root.path()).unwrap();

        journal.discard("tx-discard").unwrap();

        assert!(!transaction_dir.exists());
    }

    #[test]
    fn scan_on_startup_purges_committed_and_returns_preparing() {
        let undo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let journal = UndoJournal::at_dir(undo.path().to_path_buf());

        let committed_dir = journal.begin("committed", root.path()).unwrap();
        let committed = all_step_transaction("committed", &committed_dir, root.path());
        journal.commit(&committed).unwrap();
        let preparing_dir = journal.begin("preparing", root.path()).unwrap();

        let entries = journal.scan_on_startup().unwrap();

        assert!(!committed_dir.exists());
        assert_eq!(
            entries,
            vec![JournalEntry {
                id: "preparing".to_owned(),
                state: JournalState::Preparing,
                dir: preparing_dir
            }]
        );
    }

    #[test]
    fn scan_on_startup_returns_empty_for_empty_directory() {
        let undo = tempdir().unwrap();
        let journal = UndoJournal::at_dir(undo.path().to_path_buf());

        assert!(journal.scan_on_startup().unwrap().is_empty());
    }

    #[test]
    fn manifest_roundtrips_all_undo_step_variants() {
        let undo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let journal = UndoJournal::at_dir(undo.path().to_path_buf());
        let transaction_dir = undo.path().join("roundtrip");
        let transaction = all_step_transaction("roundtrip", &transaction_dir, root.path());

        journal.commit(&transaction).unwrap();

        let (state, actual) = read_transaction(&transaction_dir).unwrap();
        assert_eq!(state, JournalState::Committed);
        assert_eq!(actual, transaction);
    }

    #[test]
    fn open_uses_fyler_undo_dir_override() {
        let undo = tempdir().unwrap();
        let _env = UndoDirEnv::set(undo.path());

        let journal = UndoJournal::open().unwrap();

        assert_eq!(journal.dir, undo.path());
        assert!(undo.path().exists());
    }
}
