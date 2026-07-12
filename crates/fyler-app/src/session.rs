//! 正常終了時に保存する、安全な表示セッション状態。
//!
//! dirty bufferや実行中operationは含めず、pane layout・root・表示hintだけを
//! version付きTOMLとしてatomic temp + renameで永続化する。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fyler_core::WindowGeometry;
use fyler_core::options::{SortKey, SortOrder};
use fyler_core::pane::{PaneId, PaneLayout, SplitDirection};
use fyler_core::path::TreePath;
use fyler_fsops::scan::ScanOptions;

use crate::config;

const SESSION_FILE: &str = "session.toml";
const SESSION_TEMP_FILE: &str = "session.toml.tmp";
const SESSION_VERSION: i64 = 1;
pub const MAX_SESSION_PANES: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    pub layout: PaneLayout,
    pub active: PaneId,
    pub panes: BTreeMap<PaneId, SessionPane>,
    pub window: Option<WindowGeometry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPane {
    pub root: PathBuf,
    pub cursor: Option<TreePath>,
    pub collapsed: Vec<TreePath>,
    /// Loaded and expanded directories. Required to restore lazy baseline coverage without
    /// persisting a directory listing.
    pub expanded: Vec<TreePath>,
    pub scan_options: ScanOptions,
}

pub fn load() -> anyhow::Result<Option<SessionState>> {
    let path = config::config_dir()?.join(SESSION_FILE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()));
        }
    };
    parse(&source)
        .with_context(|| format!("Invalid session file {}", path.display()))
        .map(Some)
}

pub fn save(state: &SessionState) -> anyhow::Result<()> {
    validate(state)?;
    let directory = config::config_dir()?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("Failed to create {}", directory.display()))?;
    let destination = directory.join(SESSION_FILE);
    let contents = encode(state)?.to_string();
    atomic_write(&destination, &contents)
}

fn atomic_write(destination: &Path, contents: &str) -> anyhow::Result<()> {
    let directory = destination
        .parent()
        .context("session destination has no parent directory")?;
    let temporary = directory.join(SESSION_TEMP_FILE);
    let result = (|| -> anyhow::Result<()> {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("Failed to create {}", temporary.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("Failed to write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to sync {}", temporary.display()))?;
        fs::rename(&temporary, destination).with_context(|| {
            format!(
                "Failed to atomically replace {} with {}",
                destination.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn encode(state: &SessionState) -> anyhow::Result<toml::Table> {
    let mut table = toml::Table::new();
    table.insert("version".to_owned(), toml::Value::Integer(SESSION_VERSION));
    table.insert("active".to_owned(), pane_id_value(state.active)?);
    table.insert("layout".to_owned(), encode_layout(&state.layout)?);
    if let Some(window) = state.window {
        table.insert("window".to_owned(), encode_window(window));
    }
    let panes = state
        .panes
        .iter()
        .map(|(id, pane)| {
            let mut value = toml::Table::new();
            value.insert("id".to_owned(), pane_id_value(*id)?);
            value.insert("root".to_owned(), path_value(&pane.root)?);
            if let Some(cursor) = &pane.cursor {
                value.insert("cursor".to_owned(), tree_path_value(cursor));
            }
            value.insert("collapsed".to_owned(), tree_paths_value(&pane.collapsed));
            value.insert("expanded".to_owned(), tree_paths_value(&pane.expanded));
            value.insert(
                "show_hidden".to_owned(),
                toml::Value::Boolean(pane.scan_options.show_hidden),
            );
            value.insert(
                "sort".to_owned(),
                toml::Value::String(
                    match pane.scan_options.sort {
                        SortOrder::DirsFirst => "dirs_first",
                        SortOrder::Mixed => "mixed",
                    }
                    .to_owned(),
                ),
            );
            value.insert(
                "sort_key".to_owned(),
                toml::Value::String(
                    match pane.scan_options.key {
                        SortKey::Name => "name",
                        SortKey::Date => "date",
                        SortKey::Size => "size",
                        SortKey::Extension => "ext",
                    }
                    .to_owned(),
                ),
            );
            value.insert(
                "sort_reverse".to_owned(),
                toml::Value::Boolean(pane.scan_options.reverse),
            );
            Ok(toml::Value::Table(value))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    table.insert("panes".to_owned(), toml::Value::Array(panes));
    Ok(table)
}

fn parse(source: &str) -> anyhow::Result<SessionState> {
    let table = source.parse::<toml::Table>().context("Invalid TOML")?;
    let version = required(&table, "version")?
        .as_integer()
        .context("version must be an integer")?;
    if version != SESSION_VERSION {
        bail!("Unsupported session schema version: {version}");
    }
    let mut layout = parse_layout(required(&table, "layout")?)?;
    layout = limit_layout(&layout, MAX_SESSION_PANES).context("layout contains no panes")?;
    let retained = layout.leaves().into_iter().collect::<BTreeSet<_>>();

    let pane_values = required(&table, "panes")?
        .as_array()
        .context("panes must be an array")?;
    let mut panes = BTreeMap::new();
    for value in pane_values {
        let pane_table = value.as_table().context("each pane must be a table")?;
        let id = parse_pane_id(required(pane_table, "id")?)?;
        if !retained.contains(&id) {
            continue;
        }
        let root = PathBuf::from(
            required(pane_table, "root")?
                .as_str()
                .context("pane root must be a string")?,
        );
        if !root.is_absolute() {
            bail!("pane root must be absolute: {}", root.display());
        }
        let cursor = pane_table.get("cursor").map(parse_tree_path).transpose()?;
        let collapsed = parse_tree_paths(pane_table.get("collapsed"))?;
        let expanded = parse_tree_paths(pane_table.get("expanded"))?;
        let show_hidden = optional_bool(pane_table, "show_hidden", false)?;
        let sort = match optional_str(pane_table, "sort", "dirs_first")? {
            "dirs_first" => SortOrder::DirsFirst,
            "mixed" => SortOrder::Mixed,
            value => bail!("Invalid pane sort value: {value}"),
        };
        let key = match optional_str(pane_table, "sort_key", "name")? {
            "name" => SortKey::Name,
            "date" => SortKey::Date,
            "size" => SortKey::Size,
            "ext" => SortKey::Extension,
            value => bail!("Invalid pane sort_key value: {value}"),
        };
        let reverse = optional_bool(pane_table, "sort_reverse", false)?;
        if panes
            .insert(
                id,
                SessionPane {
                    root,
                    cursor,
                    collapsed,
                    expanded,
                    scan_options: ScanOptions {
                        show_hidden,
                        sort,
                        key,
                        reverse,
                    },
                },
            )
            .is_some()
        {
            bail!("Duplicate pane id: {id}");
        }
    }
    if panes.keys().copied().collect::<BTreeSet<_>>() != retained {
        bail!("layout and pane records do not contain the same pane IDs");
    }
    let requested_active = parse_pane_id(required(&table, "active")?)?;
    let active = if retained.contains(&requested_active) {
        requested_active
    } else {
        *retained.first().context("session contains no panes")?
    };
    let window = table.get("window").map(parse_window).transpose()?;
    let state = SessionState {
        layout,
        active,
        panes,
        window,
    };
    validate(&state)?;
    Ok(state)
}

fn validate(state: &SessionState) -> anyhow::Result<()> {
    let leaves = state.layout.leaves();
    if leaves.is_empty() || leaves.len() > MAX_SESSION_PANES {
        bail!("session must contain 1 to {MAX_SESSION_PANES} panes");
    }
    let leaf_set = leaves.iter().copied().collect::<BTreeSet<_>>();
    if leaf_set.len() != leaves.len() {
        bail!("layout contains duplicate pane IDs");
    }
    if leaf_set != state.panes.keys().copied().collect() {
        bail!("layout and pane records do not contain the same pane IDs");
    }
    if !leaf_set.contains(&state.active) {
        bail!("active pane is not present in layout");
    }
    if state.window.is_some_and(|window| !window.is_valid()) {
        bail!("session window geometry is invalid");
    }
    Ok(())
}
fn encode_window(window: WindowGeometry) -> toml::Value {
    let mut table = toml::Table::new();
    table.insert(
        "inner_width".to_owned(),
        toml::Value::Float(f64::from(window.inner_width)),
    );
    table.insert(
        "inner_height".to_owned(),
        toml::Value::Float(f64::from(window.inner_height)),
    );
    table.insert(
        "outer_x".to_owned(),
        toml::Value::Float(f64::from(window.outer_x)),
    );
    table.insert(
        "outer_y".to_owned(),
        toml::Value::Float(f64::from(window.outer_y)),
    );
    table.insert(
        "maximized".to_owned(),
        toml::Value::Boolean(window.maximized),
    );
    toml::Value::Table(table)
}

fn parse_window(value: &toml::Value) -> anyhow::Result<WindowGeometry> {
    let table = value.as_table().context("window must be a table")?;
    WindowGeometry::new(
        required_number(table, "inner_width")?,
        required_number(table, "inner_height")?,
        required_number(table, "outer_x")?,
        required_number(table, "outer_y")?,
        required(table, "maximized")?
            .as_bool()
            .context("window maximized must be a boolean")?,
    )
    .context("window geometry contains invalid values")
}

fn required_number(table: &toml::Table, key: &str) -> anyhow::Result<f32> {
    match required(table, key)? {
        toml::Value::Float(value) => Ok(*value as f32),
        toml::Value::Integer(value) => Ok(*value as f32),
        _ => bail!("window {key} must be a number"),
    }
}

pub fn retain_available_layout(
    layout: &PaneLayout,
    available: &BTreeSet<PaneId>,
) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(id) => available.contains(id).then_some(PaneLayout::Leaf(*id)),
        PaneLayout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            match (
                retain_available_layout(first, available),
                retain_available_layout(second, available),
            ) {
                (Some(first), Some(second)) => Some(PaneLayout::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(child), None) | (None, Some(child)) => Some(child),
                (None, None) => None,
            }
        }
    }
}

fn limit_layout(layout: &PaneLayout, max: usize) -> Option<PaneLayout> {
    let available = layout
        .leaves()
        .into_iter()
        .take(max)
        .collect::<BTreeSet<_>>();
    retain_available_layout(layout, &available)
}

fn encode_layout(layout: &PaneLayout) -> anyhow::Result<toml::Value> {
    let mut table = toml::Table::new();
    match layout {
        PaneLayout::Leaf(id) => {
            table.insert("kind".to_owned(), toml::Value::String("leaf".to_owned()));
            table.insert("pane".to_owned(), pane_id_value(*id)?);
        }
        PaneLayout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            table.insert("kind".to_owned(), toml::Value::String("split".to_owned()));
            table.insert(
                "direction".to_owned(),
                toml::Value::String(
                    match direction {
                        SplitDirection::Horizontal => "horizontal",
                        SplitDirection::Vertical => "vertical",
                    }
                    .to_owned(),
                ),
            );
            table.insert("ratio".to_owned(), toml::Value::Float(f64::from(*ratio)));
            table.insert("first".to_owned(), encode_layout(first)?);
            table.insert("second".to_owned(), encode_layout(second)?);
        }
    }
    Ok(toml::Value::Table(table))
}

fn parse_layout(value: &toml::Value) -> anyhow::Result<PaneLayout> {
    let table = value.as_table().context("layout node must be a table")?;
    match required(table, "kind")?
        .as_str()
        .context("layout kind must be a string")?
    {
        "leaf" => Ok(PaneLayout::Leaf(parse_pane_id(required(table, "pane")?)?)),
        "split" => {
            let direction = match required(table, "direction")?
                .as_str()
                .context("split direction must be a string")?
            {
                "horizontal" => SplitDirection::Horizontal,
                "vertical" => SplitDirection::Vertical,
                value => bail!("Invalid split direction: {value}"),
            };
            let ratio = match required(table, "ratio")? {
                toml::Value::Float(value) => *value as f32,
                toml::Value::Integer(value) => *value as f32,
                _ => bail!("split ratio must be a number"),
            };
            if !ratio.is_finite() || ratio <= 0.0 || ratio >= 1.0 {
                bail!("split ratio must be between 0 and 1");
            }
            Ok(PaneLayout::Split {
                direction,
                ratio,
                first: Box::new(parse_layout(required(table, "first")?)?),
                second: Box::new(parse_layout(required(table, "second")?)?),
            })
        }
        value => bail!("Invalid layout kind: {value}"),
    }
}

fn required<'a>(table: &'a toml::Table, key: &str) -> anyhow::Result<&'a toml::Value> {
    table
        .get(key)
        .with_context(|| format!("Missing required field: {key}"))
}

fn pane_id_value(id: PaneId) -> anyhow::Result<toml::Value> {
    Ok(toml::Value::Integer(
        i64::try_from(id.get()).context("pane id is too large")?,
    ))
}

fn parse_pane_id(value: &toml::Value) -> anyhow::Result<PaneId> {
    let value = value.as_integer().context("pane id must be an integer")?;
    let value = u64::try_from(value).context("pane id must be positive")?;
    if value == 0 {
        bail!("pane id must be positive");
    }
    Ok(PaneId::new(value))
}

fn path_value(path: &Path) -> anyhow::Result<toml::Value> {
    Ok(toml::Value::String(
        path.to_str()
            .context("session paths must be Unicode")?
            .to_owned(),
    ))
}

fn tree_path_value(path: &TreePath) -> toml::Value {
    toml::Value::Array(
        path.components()
            .iter()
            .cloned()
            .map(toml::Value::String)
            .collect(),
    )
}

fn tree_paths_value(paths: &[TreePath]) -> toml::Value {
    toml::Value::Array(paths.iter().map(tree_path_value).collect())
}

fn parse_tree_path(value: &toml::Value) -> anyhow::Result<TreePath> {
    let components = value
        .as_array()
        .context("tree path must be an array")?
        .iter()
        .map(|component| {
            let component = component
                .as_str()
                .context("path component must be a string")?;
            if component.is_empty() || component.contains('/') || component.contains('\\') {
                bail!("Invalid tree path component: {component:?}");
            }
            Ok(component.to_owned())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(TreePath::from_components(components))
}

fn parse_tree_paths(value: Option<&toml::Value>) -> anyhow::Result<Vec<TreePath>> {
    value
        .map(|value| {
            value
                .as_array()
                .context("tree path list must be an array")?
                .iter()
                .map(parse_tree_path)
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn optional_bool(table: &toml::Table, key: &str, default: bool) -> anyhow::Result<bool> {
    table
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("{key} must be a boolean"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn optional_str<'a>(
    table: &'a toml::Table,
    key: &str,
    default: &'a str,
) -> anyhow::Result<&'a str> {
    table
        .get(key)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("{key} must be a string"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn absolute_root(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{name}"))
        } else {
            PathBuf::from(format!("/tmp/{name}"))
        }
    }

    fn pane(root: &str) -> SessionPane {
        SessionPane {
            root: PathBuf::from(root),
            cursor: Some(TreePath::from_components(["日本語", "file.txt"])),
            collapsed: vec![TreePath::parse("src/nested")],
            expanded: vec![TreePath::parse("src")],
            scan_options: ScanOptions {
                show_hidden: true,
                sort: SortOrder::Mixed,
                key: SortKey::Date,
                reverse: true,
            },
        }
    }

    #[test]
    fn nested_layout_and_pane_state_roundtrip() {
        let one = PaneId::new(1);
        let two = PaneId::new(2);
        let three = PaneId::new(3);
        let layout = PaneLayout::Split {
            direction: SplitDirection::Vertical,
            ratio: 0.35,
            first: Box::new(PaneLayout::Leaf(one)),
            second: Box::new(PaneLayout::Split {
                direction: SplitDirection::Horizontal,
                ratio: 0.7,
                first: Box::new(PaneLayout::Leaf(two)),
                second: Box::new(PaneLayout::Leaf(three)),
            }),
        };
        let state = SessionState {
            layout,
            active: two,
            panes: BTreeMap::from([
                (one, pane(absolute_root("日本語").to_str().unwrap())),
                (two, pane("//server/share/長い path")),
                (three, pane(absolute_root("three").to_str().unwrap())),
            ]),
            window: WindowGeometry::new(1280.0, 720.0, 30.0, 40.0, true),
        };
        let encoded = encode(&state).unwrap().to_string();
        assert_eq!(parse(&encoded).unwrap(), state);
    }

    #[test]
    fn unknown_version_and_truncated_data_are_rejected() {
        assert!(
            parse("version = 99")
                .unwrap_err()
                .to_string()
                .contains("Unsupported")
        );
        assert!(parse("version = 1\nactive = 1").is_err());
    }

    #[test]
    fn layout_above_limit_is_pruned_during_parse() {
        let ids = (1..=5).map(PaneId::new).collect::<Vec<_>>();
        let mut layout = PaneLayout::Leaf(ids[0]);
        for (index, id) in ids.iter().copied().enumerate().skip(1) {
            layout = layout
                .split(ids[index - 1], SplitDirection::Vertical, id)
                .unwrap();
        }
        let state = SessionState {
            layout,
            active: ids[4],
            panes: ids
                .iter()
                .copied()
                .map(|id| (id, pane(absolute_root(&id.to_string()).to_str().unwrap())))
                .collect(),
            window: None,
        };
        let parsed = parse(&encode(&state).unwrap().to_string()).unwrap();
        assert_eq!(parsed.layout.leaves(), ids[..4]);
        assert_eq!(parsed.panes.len(), 4);
        assert_eq!(parsed.active, ids[0]);
    }

    #[test]
    fn one_two_and_four_pane_layouts_roundtrip() {
        for pane_count in [1_u64, 2, 4] {
            let ids = (1..=pane_count).map(PaneId::new).collect::<Vec<_>>();
            let mut layout = PaneLayout::Leaf(ids[0]);
            for (index, id) in ids.iter().copied().enumerate().skip(1) {
                layout = layout
                    .split(ids[index - 1], SplitDirection::Horizontal, id)
                    .unwrap();
            }
            let state = SessionState {
                layout,
                active: *ids.last().unwrap(),
                panes: ids
                    .iter()
                    .copied()
                    .map(|id| (id, pane(absolute_root(&id.to_string()).to_str().unwrap())))
                    .collect(),
                window: None,
            };
            assert_eq!(parse(&encode(&state).unwrap().to_string()).unwrap(), state);
        }
    }

    #[test]
    fn serialized_schema_contains_no_dirty_mode_dialog_or_inflight_state() {
        let id = PaneId::new(1);
        let state = SessionState {
            layout: PaneLayout::Leaf(id),
            active: id,
            panes: BTreeMap::from([(id, pane(absolute_root("clean").to_str().unwrap()))]),
            window: None,
        };
        let table = encode(&state).unwrap();
        let pane = table["panes"].as_array().unwrap()[0].as_table().unwrap();
        assert_eq!(
            pane.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "collapsed",
                "cursor",
                "expanded",
                "id",
                "root",
                "show_hidden",
                "sort",
                "sort_key",
                "sort_reverse",
            ])
        );
    }

    #[test]
    fn failed_atomic_replace_preserves_previous_session() {
        let directory = tempdir().unwrap();
        let destination = directory.path().join(SESSION_FILE);
        fs::write(&destination, "previous").unwrap();
        let temporary_as_directory = directory.path().join(SESSION_TEMP_FILE);
        fs::create_dir(&temporary_as_directory).unwrap();
        let result = atomic_write(&destination, "replacement");
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(destination).unwrap(), "previous");
    }
}
