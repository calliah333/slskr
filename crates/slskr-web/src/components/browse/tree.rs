use leptos::prelude::*;
use slskr_api::users::Directory;

#[derive(Clone)]
pub struct TreeNode {
    pub directory: Directory,
    pub children: Vec<TreeNode>,
    pub locked: bool,
}

fn build_node(directory: Directory, all: &[Directory], locked: bool) -> TreeNode {
    let prefix = format!("{}\\", directory.name);
    let children = all
        .iter()
        .filter(|candidate| {
            candidate.name.starts_with(&prefix) && !candidate.name[prefix.len()..].contains('\\')
        })
        .cloned()
        .map(|child| build_node(child, all, locked))
        .collect();
    TreeNode {
        directory,
        children,
        locked,
    }
}

pub fn build_tree(directories: &[Directory], locked: bool) -> Vec<TreeNode> {
    directories
        .iter()
        .filter(|directory| {
            !directories.iter().any(|parent| {
                directory
                    .name
                    .strip_prefix(&format!("{}\\", parent.name))
                    .is_some_and(|rest| !rest.contains('\\'))
            })
        })
        .cloned()
        .map(|directory| build_node(directory, directories, locked))
        .collect()
}

#[component]
pub fn DirectoryTree(
    tree: Vec<TreeNode>,
    selected: Option<String>,
    on_select: Callback<TreeNode>,
) -> impl IntoView {
    fn rows(
        nodes: Vec<TreeNode>,
        selected: Option<String>,
        on_select: Callback<TreeNode>,
        depth: usize,
    ) -> AnyView {
        view! { <>
            {nodes.into_iter().map(|node| {
                let path = node.directory.name.clone();
                let active = selected.as_deref() == Some(path.as_str());
                let label = node.directory.name.rsplit(['\\', '/']).next().unwrap_or(&node.directory.name).to_owned();
                let child_rows = (!node.children.is_empty()).then(|| rows(node.children.clone(), selected.clone(), on_select, depth + 1));
                view! { <>
                    <div class:active class="browse-tree-row" style:padding-left=format!("{}px", 10 + depth * 18)>
                        <button type="button" class="browse-tree-select" on:click=move |_| on_select.run(node.clone())>
                            <span>{if node.locked { "Locked " } else { "Folder " }}</span>{label}
                            <small>{format!(" ({} files)", node.directory.file_count)}</small>
                        </button>
                    </div>
                    {child_rows.map(|children| children)}
                </>
                }
            }).collect_view()}
        </> }.into_view().into_any()
    }
    view! { <div class="browse-tree">{rows(tree, selected, on_select, 0)}</div> }
}
