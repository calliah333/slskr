use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use slskr_types::users::Directory;

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

#[component]
pub fn DirectoryTree(
    tree: Vec<TreeNode>,
    #[prop(into)] selected: Signal<Option<String>>,
    on_select: Callback<String>,
) -> impl IntoView {
    fn rows(
        nodes: Vec<TreeNode>,
        selected: Signal<Option<String>>,
        on_select: Callback<String>,
        depth: usize,
    ) -> AnyView {
        nodes
            .into_iter()
            .map(|node| {
                let TreeNode {
                    directory,
                    children,
                    locked,
                } = node;
                let path = directory.name;
                let label = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned();
                let file_count = directory.files.len();
                let child_rows =
                    (!children.is_empty()).then(|| rows(children, selected, on_select, depth + 1));
                let active_path = path.clone();
                // Only this class binding reacts to selection changes; rows are not rebuilt.
                let active = move || selected.with(|s| s.as_deref() == Some(active_path.as_str()));
                view! {
                    <div class="browse-tree-row" class:active=active style:padding-left=format!("{}px", 10 + depth * 18)>
                        <button type="button" class="browse-tree-select" on:click=move |_| on_select.run(path.clone())>
                            <span>{if locked { "Locked " } else { "Folder " }}</span>{label}
                            <small>{format!(" ({file_count} files)")}</small>
                        </button>
                    </div>
                    {child_rows}
                }
            })
            .collect_view()
            .into_any()
    }
    view! { <div class="browse-tree">{rows(tree, selected, on_select, 0)}</div> }
}
