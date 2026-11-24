use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use super::actions::{BrowseActions, MenuAnchor};
use crate::ui::Icon;
use leptos::{
    prelude::*, reactive::computed::Selector, tachys::html::attribute::custom::custom_attribute,
};
use slskr_types::users::{Directory, File};
use wasm_bindgen::JsCast;

#[derive(Clone)]
pub struct TreeNode {
    pub directory: Directory,
    pub children: Vec<TreeNode>,
    pub locked: bool,
}

/// Detects the peer's path separator from the first directory name containing one,
/// defaulting to `\` like upstream.
pub fn detect_separator<'a>(names: impl IntoIterator<Item = &'a str>) -> char {
    names
        .into_iter()
        .find_map(|name| {
            if name.contains('\\') {
                Some('\\')
            } else if name.contains('/') {
                Some('/')
            } else {
                None
            }
        })
        .unwrap_or('\\')
}

fn parent_key(name: &str, separator: char) -> &str {
    name.rfind(separator).map_or("", |index| &name[..index])
}

/// Builds the directory tree in a single pass by grouping each directory under its
/// parent path (upstream `Browse.getDirectoryTree`). Each entry carries its locked flag.
pub fn build_tree(directories: Vec<(Directory, bool)>, separator: char) -> Vec<TreeNode> {
    let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
    let names: HashSet<String> = directories.iter().map(|(d, _)| d.name.clone()).collect();
    let mut roots = Vec::new();
    for (index, (directory, _)) in directories.iter().enumerate() {
        let parent = parent_key(&directory.name, separator);
        if names.contains(parent) {
            by_parent.entry(parent.to_owned()).or_default().push(index);
        } else {
            roots.push(index);
        }
    }
    let mut slots: Vec<Option<(Directory, bool)>> = directories.into_iter().map(Some).collect();
    // Each slot is taken exactly once, so the build is O(N) and never clones files.
    fn build(
        index: usize,
        slots: &mut [Option<(Directory, bool)>],
        by_parent: &mut HashMap<String, Vec<usize>>,
    ) -> Option<TreeNode> {
        let (directory, locked) = slots[index].take()?;
        let children = by_parent
            .remove(&directory.name)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|child| build(child, slots, by_parent))
            .collect();
        Some(TreeNode {
            directory,
            children,
            locked,
        })
    }
    roots
        .into_iter()
        .filter_map(|index| build(index, &mut slots, &mut by_parent))
        .collect()
}

/// A folder of [`Shares`]. Folders are stored in pre-order, so the subtree of the folder at
/// `index` is `index..end`.
#[derive(Clone, Debug)]
pub struct FlatNode {
    /// Full remote path.
    pub path: String,
    /// Last path component (the full path for a top-level folder).
    pub label: String,
    /// Lowercase `path`, for the folder filter.
    search: String,
    pub depth: usize,
    pub parent: Option<usize>,
    pub end: usize,
    pub locked: bool,
}

/// A browse result laid out for display: folders in pre-order (sorted by name) and the
/// files directly in each.
pub struct Shares {
    pub nodes: Vec<FlatNode>,
    pub files: Vec<Vec<File>>,
    pub separator: char,
}

impl Shares {
    pub fn new(mut tree: Vec<TreeNode>, separator: char) -> Self {
        fn sort(nodes: &mut [TreeNode]) {
            nodes.sort_by_cached_key(|node| node.directory.name.to_lowercase());
        }
        fn flatten(
            node: TreeNode,
            depth: usize,
            parent: Option<usize>,
            separator: char,
            shares: &mut Shares,
        ) {
            let TreeNode {
                directory,
                mut children,
                locked,
            } = node;
            let index = shares.nodes.len();
            let label = match parent {
                None => directory.name.clone(),
                Some(_) => directory
                    .name
                    .rsplit(separator)
                    .next()
                    .unwrap_or(&directory.name)
                    .to_owned(),
            };
            shares.nodes.push(FlatNode {
                search: directory.name.to_lowercase(),
                path: directory.name,
                label,
                depth,
                parent,
                end: index + 1,
                locked,
            });
            shares.files.push(directory.files);
            sort(&mut children);
            for child in children {
                flatten(child, depth + 1, Some(index), separator, shares);
            }
            shares.nodes[index].end = shares.nodes.len();
        }
        let mut shares = Shares {
            nodes: Vec::new(),
            files: Vec::new(),
            separator,
        };
        sort(&mut tree);
        for root in tree {
            flatten(root, 0, None, separator, &mut shares);
        }
        shares
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    fn siblings_from(&self, first: usize, end: usize) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(Some(first).filter(|&index| index < end), move |&index| {
            Some(self.nodes[index].end).filter(|&next| next < end)
        })
    }

    pub fn roots(&self) -> impl Iterator<Item = usize> + '_ {
        self.siblings_from(0, self.len())
    }

    pub fn children(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        self.siblings_from(index + 1, self.nodes[index].end)
    }

    pub fn has_children(&self, index: usize) -> bool {
        self.nodes[index].end > index + 1
    }

    /// The folder's ancestors and the folder itself, top-level folder first.
    pub fn lineage(&self, index: usize) -> Vec<usize> {
        let mut chain: Vec<usize> =
            std::iter::successors(Some(index), |&index| self.nodes[index].parent).collect();
        chain.reverse();
        chain
    }

    /// Full remote path of a file in the folder at `index`.
    pub fn remote_path(&self, index: usize, file: &File) -> String {
        format!(
            "{}{}{}",
            self.nodes[index].path, self.separator, file.filename
        )
    }

    /// Downloadable files (folder, file index) of a folder, and of its subfolders when
    /// `recursive`. Locked folders are skipped.
    pub fn folder_files(&self, index: usize, recursive: bool) -> Vec<(usize, usize)> {
        let end = if recursive {
            self.nodes[index].end
        } else {
            index + 1
        };
        (index..end)
            .filter(|&folder| !self.nodes[folder].locked)
            .flat_map(|folder| (0..self.files[folder].len()).map(move |file| (folder, file)))
            .collect()
    }

    /// Count and total size of [`Shares::folder_files`].
    pub fn folder_totals(&self, index: usize, recursive: bool) -> (usize, u64) {
        let files = self.folder_files(index, recursive);
        let size = files
            .iter()
            .map(|&(folder, file)| self.files[folder][file].size)
            .sum();
        (files.len(), size)
    }

    /// Folders whose path contains `query` (lowercase), in tree order.
    pub fn matching(&self, query: &str) -> impl Iterator<Item = usize> + '_ {
        let query = query.to_owned();
        self.nodes
            .iter()
            .enumerate()
            .filter(move |(_, node)| node.search.contains(&query))
            .map(|(index, _)| index)
    }

    /// Initially expanded folders: the top level, and chains of single subfolders below it.
    fn initially_expanded(&self) -> Vec<bool> {
        let mut expanded = vec![false; self.len()];
        for index in 0..self.len() {
            expanded[index] = match self.nodes[index].parent {
                None => true,
                // The only child of an expanded folder.
                Some(parent) => {
                    expanded[parent]
                        && index == parent + 1
                        && self.nodes[index].end == self.nodes[parent].end
                }
            };
        }
        expanded
    }
}

/// Most filter matches rendered at once.
const FILTER_LIMIT: usize = 300;

struct TreeContext {
    shares: Arc<Shares>,
    expanded: StoredValue<Vec<RwSignal<bool>>>,
    selector: Selector<Option<usize>>,
    selected: Signal<Option<usize>>,
    on_select: Callback<usize>,
    actions: BrowseActions,
}

impl TreeContext {
    fn expanded(&self, index: usize) -> RwSignal<bool> {
        self.expanded.with_value(|expanded| expanded[index])
    }
}

/// Shows the folders of `shares` as a keyboard navigable tree (`role="tree"`).
///
/// Children are rendered only while their folder is expanded, and a selection change
/// touches only the two rows involved, so large shares stay responsive. While `filter` is
/// not empty the matching folders are listed flat instead.
#[component]
pub fn DirectoryTree(
    shares: Arc<Shares>,
    #[prop(into)] selected: Signal<Option<usize>>,
    on_select: Callback<usize>,
    #[prop(into)] filter: Signal<String>,
) -> impl IntoView {
    let expanded = StoredValue::new(
        shares
            .initially_expanded()
            .into_iter()
            .map(RwSignal::new)
            .collect::<Vec<_>>(),
    );
    let context = Arc::new(TreeContext {
        shares: shares.clone(),
        expanded,
        selector: Selector::new(move || selected.get()),
        selected,
        on_select,
        actions: expect_context::<BrowseActions>(),
    });
    // Selecting a folder (from the filter, a breadcrumb or the parent) reveals it.
    let reveal = context.clone();
    Effect::new(move |_| {
        if let Some(index) = reveal.selected.get() {
            for ancestor in reveal.shares.lineage(index).into_iter().rev().skip(1) {
                let signal = reveal.expanded(ancestor);
                if !signal.get_untracked() {
                    signal.set(true);
                }
            }
        }
    });
    let query = Memo::new(move |_| filter.with(|filter| filter.trim().to_lowercase()));
    let filtering = Memo::new(move |_| query.with(|query| !query.is_empty()));
    let keyboard = context.clone();
    let on_keydown = move |event: web_sys::KeyboardEvent| {
        handle_key(&event, &keyboard, filtering.get_untracked());
    };
    view! {
        <div class="browse-tree" role="tree" aria-label="Folders" on:keydown=on_keydown>
            {move || if filtering.get() {
                let context = context.clone();
                (move || {
                    let (rows, total) = query.with(|query| {
                        let mut matches = context.shares.matching(query);
                        let rows: Vec<usize> = matches.by_ref().take(FILTER_LIMIT).collect();
                        (rows.clone(), rows.len() + matches.count())
                    });
                    let first = rows.first().copied();
                    let shown = rows.len();
                    let items = rows
                        .into_iter()
                        .map(|index| tree_row(index, &context, true, first == Some(index)))
                        .collect_view();
                    view! {
                        {items}
                        {(total == 0).then(|| view! { <p class="browse-tree__note">"No folders match."</p> })}
                        {(total > shown).then(|| view! {
                            <p class="browse-tree__note">{format!("Showing {shown} of {total} folders; refine the filter to see more.")}</p>
                        })}
                    }
                })
                .into_any()
            } else {
                let first = context.shares.roots().next();
                context
                    .shares
                    .roots()
                    .map(|index| tree_row(index, &context, false, first == Some(index)))
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}

/// One folder row, followed (when expanded and not `flat`) by its children's rows. All rows
/// are siblings in the DOM so arrow keys can move between them.
fn tree_row(index: usize, context: &Arc<TreeContext>, flat: bool, first: bool) -> AnyView {
    let node = &context.shares.nodes[index];
    let has_children = !flat && context.shares.has_children(index);
    let expanded = context.expanded(index);
    let locked = node.locked;
    let file_count = context.shares.files[index].len();
    let depth = if flat { 0 } else { node.depth };
    // The view macro has no `aria-level` attribute.
    let level = custom_attribute("aria-level", (depth + 1).to_string());
    let label = node.label.clone();
    let path = node.path.clone();
    // In the filter list, the parent path gives context.
    let location = (flat && node.parent.is_some()).then(|| {
        node.path[..node.path.len() - node.label.len()]
            .trim_end_matches(context.shares.separator)
            .to_owned()
    });
    let selector = context.selector.clone();
    let is_selected = move || selector.selected(&Some(index));
    let selected = context.selected;
    let tabindex = {
        let is_selected = is_selected.clone();
        move || {
            if is_selected() || (first && selected.with(Option::is_none)) {
                "0"
            } else {
                "-1"
            }
        }
    };
    let on_select = context.on_select;
    let actions = context.actions;
    let icon = move || {
        if locked {
            "lock"
        } else if has_children && expanded.get() {
            "folder-open"
        } else {
            "folder"
        }
    };
    let children = has_children.then(|| {
        let context = context.clone();
        move || {
            expanded.get().then(|| {
                context
                    .shares
                    .children(index)
                    .map(|child| tree_row(child, &context, false, false))
                    .collect_view()
            })
        }
    });
    let aria_selected = {
        let is_selected = is_selected.clone();
        move || is_selected().to_string()
    };
    view! {
        <div
            class="browse-tree__row"
            class:is-selected=is_selected
            class:is-locked=locked
            role="treeitem"
            {..level}
            data-index=index
            aria-selected=aria_selected
            aria-expanded=move || has_children.then(|| expanded.get().to_string())
            tabindex=tabindex
            style=format!("--depth: {depth}")
            title=path
            on:click=move |_| {
                on_select.run(index);
                if has_children && !expanded.get_untracked() {
                    expanded.set(true);
                }
            }
            on:contextmenu=move |event| {
                event.prevent_default();
                actions.open_folder_menu(index, MenuAnchor::Pointer(event));
            }
        >
            {if has_children {
                view! {
                    <span class="browse-tree__chevron" aria-hidden="true" on:click=move |event| {
                        event.stop_propagation();
                        expanded.update(|open| *open = !*open);
                    }><Icon name="chevron-right" size="sm"/></span>
                }.into_any()
            } else {
                view! { <span class="browse-tree__chevron" aria-hidden="true"></span> }.into_any()
            }}
            <Icon name=Signal::derive(move || icon().to_owned()) class="browse-tree__icon"/>
            <span class="browse-tree__label">
                <span class="browse-tree__name">{label}</span>
                {location.map(|location| view! { <span class="browse-tree__location">{location}</span> })}
            </span>
            {locked.then(|| view! { <span class="sr-only">", locked"</span> })}
            {(file_count > 0).then(|| view! {
                <span class="browse-tree__count" title=format!("{file_count} files")>{file_count}</span>
            })}
            // Pointer access to the folder menu; keyboard users press the context-menu key.
            <button type="button" class="browse-tree__more" tabindex="-1" aria-label="Folder actions"
                title="Folder actions" aria-haspopup="menu"
                on:click=move |event| {
                    event.stop_propagation();
                    if let Some(button) = event.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                        actions.open_folder_menu(index, MenuAnchor::Below(button));
                    }
                }>
                <Icon name="ellipsis"/>
            </button>
        </div>
        {children}
    }
    .into_any()
}

fn row_index(element: &web_sys::Element) -> Option<usize> {
    element.get_attribute("data-index")?.parse().ok()
}

fn focus(element: Option<web_sys::Element>) {
    if let Some(element) =
        element.and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = element.focus();
    }
}

/// Tree keyboard support: Up/Down/Home/End move, Right expands or enters, Left collapses
/// or goes to the parent, Enter/Space select.
fn handle_key(event: &web_sys::KeyboardEvent, context: &TreeContext, flat: bool) {
    let Some(row) = event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .filter(|element| element.get_attribute("role").as_deref() == Some("treeitem"))
    else {
        return;
    };
    let Some(index) = row_index(&row) else { return };
    let next_row = |element: &web_sys::Element| {
        std::iter::successors(element.next_element_sibling(), |e| e.next_element_sibling())
            .find(|e| row_index(e).is_some())
    };
    let previous_row = |element: &web_sys::Element| {
        std::iter::successors(element.previous_element_sibling(), |e| {
            e.previous_element_sibling()
        })
        .find(|e| row_index(e).is_some())
    };
    let has_children = !flat && context.shares.has_children(index);
    let expanded = context.expanded(index);
    match event.key().as_str() {
        "ArrowDown" => focus(next_row(&row)),
        "ArrowUp" => focus(previous_row(&row)),
        "Home" => focus(
            row.parent_element()
                .and_then(|tree| tree.first_element_child()),
        ),
        "End" => focus(
            row.parent_element()
                .and_then(|tree| tree.last_element_child())
                .and_then(|last| {
                    if row_index(&last).is_some() {
                        Some(last)
                    } else {
                        previous_row(&last)
                    }
                }),
        ),
        "ArrowRight" if has_children => {
            if expanded.get_untracked() {
                focus(next_row(&row));
            } else {
                expanded.set(true);
            }
        }
        "ArrowLeft" if !flat => {
            if has_children && expanded.get_untracked() {
                expanded.set(false);
            } else if let Some(parent) = context.shares.nodes[index].parent {
                focus(
                    std::iter::successors(previous_row(&row), |e| previous_row(e))
                        .find(|e| row_index(e) == Some(parent)),
                );
            }
        }
        "Enter" | " " => context.on_select.run(index),
        "ContextMenu" => context
            .actions
            .open_folder_menu(index, MenuAnchor::Below(row)),
        "F10" if event.shift_key() => context
            .actions
            .open_folder_menu(index, MenuAnchor::Below(row)),
        _ => return,
    }
    event.prevent_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str, files: usize) -> (Directory, bool) {
        let files = (0..files)
            .map(|index| File {
                filename: format!("{index}.mp3"),
                ..Default::default()
            })
            .collect();
        (
            Directory {
                name: name.to_owned(),
                files,
            },
            false,
        )
    }

    #[test]
    fn flattens_in_sorted_pre_order() {
        let tree = build_tree(
            vec![
                directory("@@a\\Music\\B", 1),
                directory("@@a\\Music", 0),
                directory("@@a\\Music\\A", 2),
                directory("@@a\\Music\\A\\CD1", 3),
                directory("@@b", 0),
            ],
            '\\',
        );
        let shares = Shares::new(tree, '\\');
        let paths: Vec<&str> = shares.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "@@a\\Music",
                "@@a\\Music\\A",
                "@@a\\Music\\A\\CD1",
                "@@a\\Music\\B",
                "@@b"
            ]
        );
        assert_eq!(shares.roots().collect::<Vec<_>>(), [0, 4]);
        assert_eq!(shares.children(0).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(shares.lineage(2), [0, 1, 2]);
        assert_eq!(shares.nodes[0].label, "@@a\\Music");
        assert_eq!(shares.nodes[2].label, "CD1");
        assert_eq!(shares.files[2].len(), 3);
        assert_eq!(shares.matching("cd").collect::<Vec<_>>(), [2]);
        assert_eq!(
            shares.initially_expanded(),
            [true, false, false, false, true]
        );
    }
}
