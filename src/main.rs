use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::stdout;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueHint};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use clap_complete::{CompleteEnv, Shell};
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use rusqlite::{params, types::Value, Connection, ErrorCode};
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Select and export a Zotero collection branch"
)]
struct Cli {
    #[arg(
        long,
        help = "Path to Zotero data directory (contains zotero.sqlite and storage)",
        value_hint = ValueHint::DirPath,
        required_unless_present = "generate_completion"
    )]
    zotero_base_path: Option<PathBuf>,

    #[arg(
        long,
        help = "Directory where the selected collection branch is copied",
        value_hint = ValueHint::DirPath,
        required_unless_present = "generate_completion"
    )]
    destination_dir: Option<PathBuf>,

    #[arg(
        long,
        help = "Collection or subcollection path to export, using forward slashes, e.g. \"Parent/Subcollection Name\"",
        add = ArgValueCompleter::new(collection_path_completer)
    )]
    collection: Option<String>,

    #[arg(
        long = "tag",
        help = "Only show/export items matching this Zotero tag. Repeat for AND matching: items must have every selected tag.",
        add = ArgValueCompleter::new(tag_completer)
    )]
    tags: Vec<String>,

    #[arg(
        long,
        help = "Copy all files even if they already exist in the destination directory"
    )]
    force_export_all: bool,

    #[arg(
        long,
        value_enum,
        help = "Print shell completion script to stdout",
        conflicts_with_all = ["zotero_base_path", "destination_dir", "collection", "force_export_all", "tags"]
    )]
    generate_completion: Option<Shell>,
}

#[derive(Debug, Clone)]
struct CollectionNode {
    id: i64,
    name: String,
    parent: Option<i64>,
    children: Vec<i64>,
}

#[derive(Debug, Clone)]
struct DisplayNode {
    id: i64,
    depth: usize,
    path_parts: Vec<String>,
}

#[derive(Debug, Clone)]
struct ExportSelection {
    node: DisplayNode,
    force_export_all: bool,
    tags: Vec<String>,
}

#[derive(Debug, Clone)]
struct TagMatch {
    name: String,
    score: usize,
}

struct TuiGuard;

impl TuiGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen).context("failed to enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    if let Some(shell) = cli.generate_completion {
        std::env::set_var("COMPLETE", completion_shell_name(shell));
        CompleteEnv::with_factory(Cli::command).complete();
        let _ = stdout();
        return Ok(());
    }

    let zotero_base_path = cli
        .zotero_base_path
        .context("--zotero-base-path is required unless --generate-completion is used")?;
    let destination_dir = cli
        .destination_dir
        .context("--destination-dir is required unless --generate-completion is used")?;

    let db_path = zotero_base_path.join("zotero.sqlite");
    let storage_path = zotero_base_path.join("storage");

    if !db_path.exists() {
        anyhow::bail!("zotero.sqlite not found at {}", db_path.display());
    }
    if !storage_path.exists() {
        anyhow::bail!("storage directory not found at {}", storage_path.display());
    }

    let conn = open_zotero_connection(&db_path)?;
    let tree = load_collection_tree(&conn)?;

    if tree.is_empty() {
        anyhow::bail!("No collections found in Zotero library.");
    }

    let tag_names = load_tag_names(&conn)?;
    let normalized_tags = normalize_tags(cli.tags)?;
    let display_nodes = flatten_tree(&tree);
    let filtered_display_nodes =
        filter_display_nodes_by_tags(&conn, &tree, &display_nodes, &normalized_tags)?;
    if !normalized_tags.is_empty() && filtered_display_nodes.is_empty() {
        anyhow::bail!(
            "No collections contain items matching tag filter: {}",
            normalized_tags.join(", ")
        );
    }

    let selection = if let Some(collection_path) = cli.collection.as_deref() {
        resolve_collection_selection(
            &filtered_display_nodes,
            collection_path,
            cli.force_export_all,
            normalized_tags.clone(),
        )?
    } else {
        let selection =
            run_collection_picker(&conn, &tree, &display_nodes, &tag_names, normalized_tags)?;
        let Some(selection) = selection else {
            println!("No collection selected. Exiting.");
            return Ok(());
        };
        selection
    };
    let selected = selection.node;

    let descendants = gather_descendants(&tree, selected.id);
    let file_index = index_storage_files(&storage_path)?;

    let mut copied = 0usize;
    let mut skipped_existing = 0usize;
    let mut missing = 0usize;

    for col_id in descendants {
        let relative_dir = build_relative_path_from_selected(&tree, selected.id, col_id)?;
        let export_dir = destination_dir.join(relative_dir);
        fs::create_dir_all(&export_dir)
            .with_context(|| format!("failed to create destination: {}", export_dir.display()))?;

        let item_ids = get_item_ids_for_collection(&conn, col_id, &selection.tags)?;
        let attachment_names = get_pdf_attachment_filenames(&conn, &item_ids)?;

        for filename in attachment_names {
            if let Some(src) = file_index.get(&filename) {
                let dst = export_dir.join(&filename);
                if !selection.force_export_all && dst.exists() {
                    skipped_existing += 1;
                    continue;
                }
                fs::copy(src, &dst).with_context(|| {
                    format!("failed to copy {} -> {}", src.display(), dst.display())
                })?;
                copied += 1;
            } else {
                missing += 1;
            }
        }
    }

    println!(
        "Export complete: {} file(s) copied, {} file(s) skipped because they already exist, {} attachment(s) not found in storage.",
        copied, skipped_existing, missing
    );
    println!("Selected collection: {}", selected.path_parts.join("/"));
    println!("Destination: {}", destination_dir.display());
    println!(
        "Mode: {}",
        if selection.force_export_all {
            "force export all"
        } else {
            "skip files already present in destination"
        }
    );
    if !selection.tags.is_empty() {
        println!("Tag filter: all of [{}]", selection.tags.join(", "));
    }

    Ok(())
}

fn collection_path_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(zotero_base_path) = completion_zotero_base_path() else {
        return Vec::new();
    };

    let db_path = zotero_base_path.join("zotero.sqlite");
    if !db_path.is_file() {
        return Vec::new();
    }

    let conn = match Connection::open(&db_path) {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };

    let tree = match load_collection_tree(&conn) {
        Ok(tree) => tree,
        Err(_) => return Vec::new(),
    };

    let prefix = current.to_string_lossy();
    flatten_tree(&tree)
        .into_iter()
        .map(|node| node.path_parts.join("/"))
        .filter(|path| path.starts_with(prefix.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

fn tag_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(zotero_base_path) = completion_zotero_base_path() else {
        return Vec::new();
    };

    let db_path = zotero_base_path.join("zotero.sqlite");
    if !db_path.is_file() {
        return Vec::new();
    }

    let conn = match Connection::open(&db_path) {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };

    let prefix = current.to_string_lossy().to_lowercase();
    match load_tag_names(&conn) {
        Ok(tags) => tags
            .into_iter()
            .filter(|tag| tag.to_lowercase().starts_with(prefix.as_str()))
            .map(CompletionCandidate::new)
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn open_zotero_connection(db_path: &Path) -> Result<Connection> {
    Connection::open(db_path).map_err(|err| {
        if matches!(
            err.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ) {
            anyhow::anyhow!(
                "failed to open Zotero DB: {}. The database appears to be locked. Shut down Zotero and try again.",
                db_path.display()
            )
        } else {
            anyhow::anyhow!("failed to open Zotero DB: {}: {}", db_path.display(), err)
        }
    })
}

fn completion_shell_name(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => "bash",
        Shell::Elvish => "elvish",
        Shell::Fish => "fish",
        Shell::PowerShell => "powershell",
        Shell::Zsh => "zsh",
        _ => "bash",
    }
}

fn completion_zotero_base_path() -> Option<PathBuf> {
    let mut args = std::env::args_os();
    while let Some(arg) = args.next() {
        if arg == "--zotero-base-path" {
            return args.next().map(PathBuf::from);
        }

        let arg = arg.to_string_lossy();
        let prefix = "--zotero-base-path=";
        if let Some(value) = arg.strip_prefix(prefix) {
            return Some(PathBuf::from(value));
        }
    }

    None
}

fn load_collection_tree(conn: &Connection) -> Result<HashMap<i64, CollectionNode>> {
    let mut stmt =
        conn.prepare("SELECT collectionID, collectionName, parentCollectionID FROM collections")?;

    let rows = stmt.query_map([], |row| {
        Ok(CollectionNode {
            id: row.get::<_, i64>(0)?,
            name: row.get::<_, String>(1)?,
            parent: row.get::<_, Option<i64>>(2)?,
            children: Vec::new(),
        })
    })?;

    let mut tree = HashMap::new();
    for row in rows {
        let node = row?;
        tree.insert(node.id, node);
    }

    let ids: Vec<i64> = tree.keys().copied().collect();
    for id in ids {
        let parent = tree.get(&id).and_then(|n| n.parent);
        if let Some(parent_id) = parent {
            if let Some(parent_node) = tree.get_mut(&parent_id) {
                parent_node.children.push(id);
            }
        }
    }

    let name_index: HashMap<i64, String> = tree
        .iter()
        .map(|(id, node)| (*id, node.name.clone()))
        .collect();

    for node in tree.values_mut() {
        node.children.sort_by_key(|child_id| {
            name_index
                .get(child_id)
                .cloned()
                .unwrap_or_else(String::new)
        });
    }

    Ok(tree)
}

fn flatten_tree(tree: &HashMap<i64, CollectionNode>) -> Vec<DisplayNode> {
    let mut roots: Vec<i64> = tree
        .values()
        .filter(|n| n.parent.is_none())
        .map(|n| n.id)
        .collect();
    roots.sort_by_key(|id| tree.get(id).map(|n| n.name.clone()).unwrap_or_default());

    let mut out = Vec::new();
    for root in roots {
        flatten_from(tree, root, 0, Vec::new(), &mut out);
    }
    out
}

fn flatten_from(
    tree: &HashMap<i64, CollectionNode>,
    id: i64,
    depth: usize,
    mut path_parts: Vec<String>,
    out: &mut Vec<DisplayNode>,
) {
    let Some(node) = tree.get(&id) else {
        return;
    };

    path_parts.push(node.name.clone());
    out.push(DisplayNode {
        id,
        depth,
        path_parts: path_parts.clone(),
    });

    for &child_id in &node.children {
        flatten_from(tree, child_id, depth + 1, path_parts.clone(), out);
    }
}

fn resolve_collection_selection(
    nodes: &[DisplayNode],
    collection_path: &str,
    force_export_all: bool,
    tags: Vec<String>,
) -> Result<ExportSelection> {
    let normalized = normalize_collection_path(collection_path);
    let node = nodes
        .iter()
        .find(|node| node.path_parts == normalized)
        .cloned()
        .with_context(|| {
            format!(
                "collection not found: {}. Use forward slashes between collection names, for example \"Parent/Subcollection Name\"",
                collection_path
            )
        })?;

    Ok(ExportSelection {
        node,
        force_export_all,
        tags,
    })
}

fn normalize_collection_path(collection_path: &str) -> Vec<String> {
    collection_path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }

        let key = tag.to_lowercase();
        if seen.insert(key) {
            out.push(tag.to_owned());
        }
    }

    Ok(out)
}

fn load_tag_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT name FROM tags
         WHERE name IS NOT NULL AND TRIM(name) != ''
         ORDER BY lower(name), name",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn filter_display_nodes_by_tags(
    conn: &Connection,
    tree: &HashMap<i64, CollectionNode>,
    nodes: &[DisplayNode],
    tags: &[String],
) -> Result<Vec<DisplayNode>> {
    if tags.is_empty() {
        return Ok(nodes.to_vec());
    }

    let mut out = Vec::new();
    for node in nodes {
        let mut has_match = false;
        for col_id in gather_descendants(tree, node.id) {
            if !get_item_ids_for_collection(conn, col_id, tags)?.is_empty() {
                has_match = true;
                break;
            }
        }

        if has_match {
            out.push(node.clone());
        }
    }

    Ok(out)
}

fn run_collection_picker(
    conn: &Connection,
    tree: &HashMap<i64, CollectionNode>,
    nodes: &[DisplayNode],
    tag_names: &[String],
    initial_tags: Vec<String>,
) -> Result<Option<ExportSelection>> {
    let _guard = TuiGuard::new()?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to init terminal")?;

    let mut selected = 0usize;
    let mut tag_selected = 0usize;
    let mut tag_query = String::new();
    let mut tag_mode = false;
    let mut selected_tags = initial_tags;
    let mut visible_nodes = filter_display_nodes_by_tags(conn, tree, nodes, &selected_tags)?;
    let mut force_export_all = false;
    let mut state = ListState::default();
    state.select(Some(selected));

    loop {
        if selected >= visible_nodes.len() {
            selected = visible_nodes.len().saturating_sub(1);
        }
        state.select((!visible_nodes.is_empty()).then_some(selected));
        let tag_matches = fuzzy_tag_matches(tag_names, &tag_query);
        if tag_selected >= tag_matches.len() {
            tag_selected = tag_matches.len().saturating_sub(1);
        }

        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Length(2), Constraint::Min(1)])
                .split(area);

            let mode = if force_export_all {
                "Force export all"
            } else {
                "Skip existing files"
            };
            let help_text = if tag_mode {
                format!(
                    "Tag filter uses AND. Type to fuzzy find, Backspace edits, Enter toggles tag, Esc returns | Mode: {}",
                    mode
                )
            } else {
                format!(
                    "Choose collection: Up/Down (or j/k), t tags (AND), c clear tags, f force, Enter export, q/Esc quit | Mode: {}",
                    mode
                )
            };
            let help = Paragraph::new(help_text)
            .style(Style::default().fg(Color::Cyan));
            frame.render_widget(help, chunks[0]);

            let tag_status = if selected_tags.is_empty() {
                "Tags: none".to_owned()
            } else {
                format!("Tags: all of [{}]", selected_tags.join(", "))
            };
            let query_status = if tag_mode {
                format!(" | Query: {}", tag_query)
            } else {
                format!(" | Visible collections: {}", visible_nodes.len())
            };
            frame.render_widget(
                Paragraph::new(format!("{}{}", tag_status, query_status))
                    .style(Style::default().fg(Color::Yellow)),
                chunks[1],
            );

            if tag_mode {
                let items: Vec<ListItem> = tag_matches
                    .iter()
                    .map(|tag_match| {
                        let selected_marker = if selected_tags
                            .iter()
                            .any(|tag| tag.eq_ignore_ascii_case(&tag_match.name))
                        {
                            "[x] "
                        } else {
                            "[ ] "
                        };
                        ListItem::new(format!("{}{}", selected_marker, tag_match.name))
                    })
                    .collect();

                let mut tag_state = ListState::default();
                tag_state.select((!tag_matches.is_empty()).then_some(tag_selected));
                let list = List::new(items)
                    .block(Block::default().title("Zotero Tags").borders(Borders::ALL))
                    .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                    .highlight_symbol("-> ");

                frame.render_stateful_widget(list, chunks[2], &mut tag_state);
                return;
            }

            let items: Vec<ListItem> = visible_nodes
                .iter()
                .map(|n| {
                    let indent = "  ".repeat(n.depth);
                    let label = n.path_parts.last().map_or("", String::as_str);
                    ListItem::new(format!("{}{}", indent, label))
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("Zotero Collections")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol("-> ");

            frame.render_stateful_widget(list, chunks[2], &mut state);
        })?;

        if event::poll(std::time::Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if tag_mode {
                    match key.code {
                        KeyCode::Esc => tag_mode = false,
                        KeyCode::Down | KeyCode::Char('j') => {
                            tag_selected =
                                (tag_selected + 1).min(tag_matches.len().saturating_sub(1));
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            tag_selected = tag_selected.saturating_sub(1);
                        }
                        KeyCode::Backspace => {
                            tag_query.pop();
                            tag_selected = 0;
                        }
                        KeyCode::Enter => {
                            if let Some(tag_match) = tag_matches.get(tag_selected) {
                                toggle_tag(&mut selected_tags, &tag_match.name);
                                visible_nodes = filter_display_nodes_by_tags(
                                    conn,
                                    tree,
                                    nodes,
                                    &selected_tags,
                                )?;
                                selected = 0;
                            }
                        }
                        KeyCode::Char(ch) => {
                            tag_query.push(ch);
                            tag_selected = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(visible_nodes.len().saturating_sub(1));
                        state.select(Some(selected));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        state.select(Some(selected));
                    }
                    KeyCode::Char('f') => {
                        force_export_all = !force_export_all;
                    }
                    KeyCode::Char('t') => {
                        tag_mode = true;
                        tag_selected = 0;
                    }
                    KeyCode::Char('c') => {
                        selected_tags.clear();
                        visible_nodes =
                            filter_display_nodes_by_tags(conn, tree, nodes, &selected_tags)?;
                        selected = 0;
                    }
                    KeyCode::Enter => {
                        return Ok(visible_nodes.get(selected).cloned().map(|node| {
                            ExportSelection {
                                node,
                                force_export_all,
                                tags: selected_tags.clone(),
                            }
                        }));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn toggle_tag(tags: &mut Vec<String>, tag_name: &str) {
    if let Some(index) = tags
        .iter()
        .position(|tag| tag.eq_ignore_ascii_case(tag_name))
    {
        tags.remove(index);
    } else {
        tags.push(tag_name.to_owned());
        tags.sort_by_key(|tag| tag.to_lowercase());
    }
}

fn fuzzy_tag_matches(tag_names: &[String], query: &str) -> Vec<TagMatch> {
    let query = query.trim();
    let mut matches: Vec<TagMatch> = tag_names
        .iter()
        .filter_map(|tag| {
            fuzzy_score(tag, query).map(|score| TagMatch {
                name: tag.clone(),
                score,
            })
        })
        .collect();

    matches.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    matches.truncate(200);
    matches
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(usize::MAX / 2);
    }

    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    if candidate_lower.contains(&query_lower) {
        return candidate_lower.find(&query_lower);
    }

    let mut score = 0usize;
    let mut last_match = None;
    let mut candidate_chars = candidate_lower.char_indices();

    for query_char in query_lower.chars() {
        let Some((idx, _)) =
            candidate_chars.find(|(_, candidate_char)| *candidate_char == query_char)
        else {
            return None;
        };

        score += idx;
        if let Some(last) = last_match {
            score += idx.saturating_sub(last + 1);
        }
        last_match = Some(idx);
    }

    Some(score + candidate_lower.len())
}

fn gather_descendants(tree: &HashMap<i64, CollectionNode>, root: i64) -> Vec<i64> {
    let mut out = Vec::new();
    gather_descendants_inner(tree, root, &mut out);
    out
}

fn gather_descendants_inner(tree: &HashMap<i64, CollectionNode>, id: i64, out: &mut Vec<i64>) {
    out.push(id);
    if let Some(node) = tree.get(&id) {
        for &child in &node.children {
            gather_descendants_inner(tree, child, out);
        }
    }
}

fn build_relative_path_from_selected(
    tree: &HashMap<i64, CollectionNode>,
    selected_root: i64,
    target: i64,
) -> Result<PathBuf> {
    let full_path = build_full_path(tree, target)?;
    let root_path = build_full_path(tree, selected_root)?;

    if full_path.starts_with(&root_path) {
        let mut rel = PathBuf::new();
        for part in full_path
            .iter()
            .skip(root_path.iter().count().saturating_sub(1))
        {
            rel.push(Path::new(part));
        }
        Ok(rel)
    } else {
        Ok(full_path)
    }
}

fn build_full_path(tree: &HashMap<i64, CollectionNode>, col_id: i64) -> Result<PathBuf> {
    let mut parts = Vec::new();
    let mut current = Some(col_id);

    while let Some(id) = current {
        let node = tree
            .get(&id)
            .with_context(|| format!("collection ID {} not found", id))?;
        parts.push(sanitize_name(&node.name));
        current = node.parent;
    }

    parts.reverse();
    let mut out = PathBuf::new();
    for p in parts {
        out.push(p);
    }
    Ok(out)
}

fn sanitize_name(name: &str) -> String {
    name.replace('/', "_").replace('\\', "_")
}

fn get_item_ids_for_collection(
    conn: &Connection,
    col_id: i64,
    tags: &[String],
) -> Result<Vec<i64>> {
    if tags.is_empty() {
        let mut stmt =
            conn.prepare("SELECT itemID FROM collectionItems WHERE collectionID = ?1")?;
        let rows = stmt.query_map(params![col_id], |row| row.get::<_, i64>(0))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        return Ok(out);
    }

    let placeholders = (0..tags.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let query = format!(
        "SELECT ci.itemID
         FROM collectionItems ci
         JOIN itemTags it ON it.itemID = ci.itemID
         JOIN tags t ON t.tagID = it.tagID
         WHERE ci.collectionID = ? AND lower(t.name) IN ({})
         GROUP BY ci.itemID
         HAVING COUNT(DISTINCT lower(t.name)) = ?",
        placeholders
    );

    let mut values = Vec::with_capacity(tags.len() + 2);
    values.push(Value::Integer(col_id));
    values.extend(tags.iter().map(|tag| Value::Text(tag.to_lowercase())));
    values.push(Value::Integer(tags.len() as i64));

    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
        row.get::<_, i64>(0)
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn get_pdf_attachment_filenames(conn: &Connection, item_ids: &[i64]) -> Result<Vec<String>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    let chunk_size = 500;

    for chunk in item_ids.chunks(chunk_size) {
        let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT path FROM itemAttachments WHERE parentItemID IN ({}) AND contentType LIKE '%pdf'",
            placeholders
        );

        let mut stmt = conn.prepare(&query)?;
        let params = rusqlite::params_from_iter(chunk.iter().copied());
        let rows = stmt.query_map(params, |row| row.get::<_, Option<String>>(0))?;

        for row in rows {
            if let Some(path) = row? {
                let stripped = path.strip_prefix("storage:").unwrap_or(&path);
                if let Some(name) = Path::new(stripped).file_name() {
                    out.push(name.to_string_lossy().to_string());
                }
            }
        }
    }

    Ok(out)
}

fn index_storage_files(storage_path: &Path) -> Result<HashMap<String, PathBuf>> {
    let mut index = HashMap::new();

    for entry in WalkDir::new(storage_path).into_iter() {
        let entry = entry.with_context(|| {
            format!(
                "failed while walking storage directory: {}",
                storage_path.display()
            )
        })?;

        if entry.file_type().is_file() {
            let name = entry.file_name().to_string_lossy().to_string();
            index
                .entry(name)
                .or_insert_with(|| entry.path().to_path_buf());
        }
    }

    Ok(index)
}
