// Copyright 2020-2024 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use itertools::Itertools;
use jj_lib::backend::{BackendResult, ChangeId, CommitId};
use jj_lib::commit::Commit;
use jj_lib::git::REMOTE_NAME_FOR_LOCAL_GIT_REPO;
use jj_lib::graph::{GraphEdge, TopoGroupedGraphIterator};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::refs::{diff_named_ref_targets, diff_named_remote_refs};
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo};
use jj_lib::revset::RevsetIteratorExt as _;
use jj_lib::rewrite::rebase_to_dest_parent;
use jj_lib::{dag_walk, op_walk, revset};

use crate::cli_util::{
    short_change_hash, short_operation_hash, CommandHelper, LogContentFormat,
    WorkspaceCommandTransaction,
};
use crate::command_error::{user_error, CommandError};
use crate::diff_util::{DiffFormatArgs, DiffRenderer};
use crate::formatter::Formatter;
use crate::graphlog::{get_graphlog, Edge};
use crate::ui::Ui;

/// Compare changes to the repository between two operations
#[derive(clap::Args, Clone, Debug)]
pub struct OperationDiffArgs {
    /// Show repository changes in this operation, compared to its parent
    #[arg(long, visible_alias = "op")]
    operation: Option<String>,
    /// Show repository changes from this operation
    #[arg(long, conflicts_with = "operation")]
    from: Option<String>,
    /// Show repository changes to this operation
    #[arg(long, conflicts_with = "operation")]
    to: Option<String>,
    /// Don't show the graph, show a flat list of modified changes
    #[arg(long)]
    no_graph: bool,
    /// Show patch of modifications to changes
    ///
    /// If the previous version has different parents, it will be temporarily
    /// rebased to the parents of the new version, so the diff is not
    /// contaminated by unrelated changes.
    #[arg(long, short = 'p')]
    patch: bool,
    #[command(flatten)]
    diff_format: DiffFormatArgs,
}

pub fn cmd_op_diff(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &OperationDiffArgs,
) -> Result<(), CommandError> {
    let workspace = command.load_workspace()?;
    let repo_loader = workspace.repo_loader();
    let head_op_str = &command.global_args().at_operation;
    let from_op;
    let to_op;
    if args.from.is_some() || args.to.is_some() {
        from_op =
            op_walk::resolve_op_for_load(repo_loader, args.from.as_ref().unwrap_or(head_op_str))?;
        to_op = op_walk::resolve_op_for_load(repo_loader, args.to.as_ref().unwrap_or(head_op_str))?;
    } else {
        to_op = op_walk::resolve_op_for_load(
            repo_loader,
            args.operation.as_ref().unwrap_or(head_op_str),
        )?;
        let to_op_parents: Vec<_> = to_op.parents().try_collect()?;
        if to_op_parents.is_empty() {
            return Err(user_error("Cannot diff operation with no parents"));
        }
        from_op = repo_loader.merge_operations(command.settings(), to_op_parents, None)?;
    }
    let with_content_format = LogContentFormat::new(ui, command.settings())?;

    let from_repo = repo_loader.load_at(&from_op)?;
    let to_repo = repo_loader.load_at(&to_op)?;

    ui.request_pager();
    ui.stdout_formatter().with_label("op_log", |formatter| {
        write!(formatter, "From operation ")?;
        write!(
            formatter.labeled("id"),
            "{}",
            short_operation_hash(from_op.id()),
        )?;
        write!(formatter, ": ")?;
        write!(
            formatter.labeled("description"),
            "{}",
            from_op.metadata().description,
        )?;
        writeln!(formatter)?;
        write!(formatter, "  To operation ")?;
        write!(
            formatter.labeled("id"),
            "{}",
            short_operation_hash(to_op.id()),
        )?;
        write!(formatter, ": ")?;
        write!(
            formatter.labeled("description"),
            "{}",
            to_op.metadata().description,
        )?;
        writeln!(formatter)?;
        writeln!(formatter)?;
        Ok(())
    })?;

    show_op_diff(
        ui,
        command,
        &from_repo,
        &to_repo,
        !args.no_graph,
        &with_content_format,
        &args.diff_format,
        args.patch,
    )
}

// Computes and shows the differences between two operations, using the given
// `Repo`s for the operations.
#[allow(clippy::too_many_arguments)]
pub fn show_op_diff(
    ui: &mut Ui,
    command: &CommandHelper,
    from_repo: &Arc<ReadonlyRepo>,
    to_repo: &Arc<ReadonlyRepo>,
    show_graph: bool,
    with_content_format: &LogContentFormat,
    diff_format_args: &DiffFormatArgs,
    patch: bool,
) -> Result<(), CommandError> {
    let diff_workspace_command =
        command.for_loaded_repo(ui, command.load_workspace()?, to_repo.clone())?;
    let diff_renderer = diff_workspace_command.diff_renderer_for_log(diff_format_args, patch)?;

    // Create a new transaction starting from `to_repo`.
    let mut workspace_command =
        command.for_loaded_repo(ui, command.load_workspace()?, to_repo.clone())?;
    let mut tx = workspace_command.start_transaction();
    // Merge index from `from_repo` to `to_repo`, so commits in `from_repo` are
    // accessible.
    tx.mut_repo().merge_index(from_repo);

    let changes = compute_operation_commits_diff(tx.mut_repo(), from_repo, to_repo)?;

    let commit_id_change_id_map: HashMap<CommitId, ChangeId> = changes
        .iter()
        .flat_map(|(change_id, modified_change)| {
            modified_change
                .added_commits
                .iter()
                .map(|commit| (commit.id().clone(), change_id.clone()))
                .chain(
                    modified_change
                        .removed_commits
                        .iter()
                        .map(|commit| (commit.id().clone(), change_id.clone())),
                )
        })
        .collect();

    let change_parents: HashMap<_, _> = changes
        .iter()
        .map(|(change_id, modified_change)| {
            let parent_change_ids = get_parent_changes(modified_change, &commit_id_change_id_map);
            (change_id.clone(), parent_change_ids)
        })
        .collect();

    // Order changes in reverse topological order.
    let ordered_changes = dag_walk::topo_order_reverse(
        changes.keys().cloned().collect_vec(),
        |change_id: &ChangeId| change_id.clone(),
        |change_id: &ChangeId| change_parents.get(change_id).unwrap().clone(),
    );

    let graph_iter = TopoGroupedGraphIterator::new(ordered_changes.iter().map(|change_id| {
        let parent_change_ids = change_parents.get(change_id).unwrap();
        (
            change_id.clone(),
            parent_change_ids
                .iter()
                .map(|parent_change_id| GraphEdge::direct(parent_change_id.clone()))
                .collect_vec(),
        )
    }));

    let mut formatter = ui.stdout_formatter();
    let formatter = formatter.as_mut();

    if !ordered_changes.is_empty() {
        writeln!(formatter, "Changed commits:")?;
        if show_graph {
            let mut graph = get_graphlog(command.settings(), formatter.raw());
            for (change_id, edges) in graph_iter {
                let modified_change = changes.get(&change_id).unwrap();
                let edges = edges
                    .iter()
                    .map(|edge| Edge::Direct(edge.target.clone()))
                    .collect_vec();

                let mut buffer = vec![];
                with_content_format.write_graph_text(
                    ui.new_formatter(&mut buffer).as_mut(),
                    |formatter| {
                        write_modified_change_summary(formatter, &tx, &change_id, modified_change)
                    },
                    || graph.width(&change_id, &edges),
                )?;
                if !buffer.ends_with(b"\n") {
                    buffer.push(b'\n');
                }
                if let Some(diff_renderer) = &diff_renderer {
                    let mut formatter = ui.new_formatter(&mut buffer);
                    show_change_diff(ui, formatter.as_mut(), &tx, diff_renderer, modified_change)?;
                }

                // TODO: customize node symbol?
                let node_symbol = "○";
                graph.add_node(
                    &change_id,
                    &edges,
                    node_symbol,
                    &String::from_utf8_lossy(&buffer),
                )?;
            }
        } else {
            for (change_id, _) in graph_iter {
                let modified_change = changes.get(&change_id).unwrap();
                write_modified_change_summary(formatter, &tx, &change_id, modified_change)?;
                if let Some(diff_renderer) = &diff_renderer {
                    show_change_diff(ui, formatter, &tx, diff_renderer, modified_change)?;
                }
            }
        }
        writeln!(formatter)?;
    }

    let changed_local_branches = diff_named_ref_targets(
        from_repo.view().local_branches(),
        to_repo.view().local_branches(),
    )
    .collect_vec();
    if !changed_local_branches.is_empty() {
        writeln!(formatter, "Changed local branches:")?;
        for (name, (from_target, to_target)) in changed_local_branches {
            writeln!(formatter, "{}:", name)?;
            write_ref_target_summary(formatter, &tx, "+", to_target)?;
            write_ref_target_summary(formatter, &tx, "-", from_target)?;
        }
        writeln!(formatter)?;
    }

    let changed_tags =
        diff_named_ref_targets(from_repo.view().tags(), to_repo.view().tags()).collect_vec();
    if !changed_tags.is_empty() {
        writeln!(formatter, "Changed tags:")?;
        for (name, (from_target, to_target)) in changed_tags {
            writeln!(formatter, "{}:", name)?;
            write_ref_target_summary(formatter, &tx, "+", to_target)?;
            write_ref_target_summary(formatter, &tx, "-", from_target)?;
        }
        writeln!(formatter)?;
    }

    let changed_remote_branches = diff_named_remote_refs(
        from_repo.view().all_remote_branches(),
        to_repo.view().all_remote_branches(),
    )
    // Skip updates to the local git repo, since they should typically be covered in
    // local branches.
    .filter(|((_, remote_name), _)| *remote_name != REMOTE_NAME_FOR_LOCAL_GIT_REPO)
    .collect_vec();
    if !changed_remote_branches.is_empty() {
        writeln!(formatter, "Changed remote branches:")?;
        let format_remote_ref_prefix = |prefix: &str, remote_ref: &RemoteRef| {
            format!(
                "{} ({})",
                prefix,
                match remote_ref.state {
                    RemoteRefState::New => "untracked",
                    RemoteRefState::Tracking => "tracked",
                }
            )
        };
        for ((name, remote_name), (from_ref, to_ref)) in changed_remote_branches {
            writeln!(formatter, "{}@{}:", name, remote_name)?;
            write_ref_target_summary(
                formatter,
                &tx,
                &format_remote_ref_prefix("+", to_ref),
                &to_ref.target,
            )?;
            write_ref_target_summary(
                formatter,
                &tx,
                &format_remote_ref_prefix("-", from_ref),
                &from_ref.target,
            )?;
        }
    }

    Ok(())
}

// Writes a summary for the given `ModifiedChange`.
fn write_modified_change_summary(
    formatter: &mut dyn Formatter,
    tx: &WorkspaceCommandTransaction,
    change_id: &ChangeId,
    modified_change: &ModifiedChange,
) -> Result<(), std::io::Error> {
    writeln!(formatter, "Change {}", short_change_hash(change_id))?;
    for commit in modified_change.added_commits.iter() {
        write!(formatter, "+")?;
        tx.write_commit_summary(formatter, commit)?;
        writeln!(formatter)?;
    }
    for commit in modified_change.removed_commits.iter() {
        write!(formatter, "-")?;
        tx.write_commit_summary(formatter, commit)?;
        writeln!(formatter)?;
    }
    Ok(())
}

// Writes a summary for the given `RefTarget`.
fn write_ref_target_summary(
    formatter: &mut dyn Formatter,
    tx: &WorkspaceCommandTransaction,
    prefix: &str,
    ref_target: &RefTarget,
) -> Result<(), CommandError> {
    if ref_target.is_absent() {
        writeln!(formatter, "{} (absent)", prefix)?;
    } else if ref_target.has_conflict() {
        for commit_id in ref_target.added_ids() {
            write!(formatter, "{} (added) ", prefix)?;
            let commit = tx.repo().store().get_commit(commit_id)?;
            tx.write_commit_summary(formatter, &commit)?;
            writeln!(formatter)?;
        }
        for commit_id in ref_target.removed_ids() {
            write!(formatter, "{} (removed) ", prefix)?;
            let commit = tx.repo().store().get_commit(commit_id)?;
            tx.write_commit_summary(formatter, &commit)?;
            writeln!(formatter)?;
        }
    } else {
        write!(formatter, "{} ", prefix)?;
        let commit_id = ref_target.as_normal().unwrap();
        let commit = tx.repo().store().get_commit(commit_id)?;
        tx.write_commit_summary(formatter, &commit)?;
        writeln!(formatter)?;
    }
    Ok(())
}

// Returns the change IDs of the parents of the given `modified_change`, which
// are the parents of all newly added commits for the change, or the parents of
// all removed commits if there are no added commits.
fn get_parent_changes(
    modified_change: &ModifiedChange,
    commit_id_change_id_map: &HashMap<CommitId, ChangeId>,
) -> Vec<ChangeId> {
    // TODO: how should we handle multiple added or removed commits?
    // This logic is probably slightly iffy.
    if !modified_change.added_commits.is_empty() {
        modified_change
            .added_commits
            .iter()
            .flat_map(|commit| commit.parent_ids())
            .filter_map(|parent_id| commit_id_change_id_map.get(parent_id).cloned())
            .unique()
            .collect_vec()
    } else {
        modified_change
            .removed_commits
            .iter()
            .flat_map(|commit| commit.parent_ids())
            .filter_map(|parent_id| commit_id_change_id_map.get(parent_id).cloned())
            .unique()
            .collect_vec()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModifiedChange {
    added_commits: Vec<Commit>,
    removed_commits: Vec<Commit>,
}

// Compute the changes in commits between two operations, returned as a
// `HashMap` from `ChangeId` to a `ModifiedChange` struct containing the added
// and removed commits for the change ID.
fn compute_operation_commits_diff(
    repo: &MutableRepo,
    from_repo: &ReadonlyRepo,
    to_repo: &ReadonlyRepo,
) -> BackendResult<IndexMap<ChangeId, ModifiedChange>> {
    let mut changes: IndexMap<ChangeId, ModifiedChange> = IndexMap::new();

    let from_heads = from_repo.view().heads().iter().cloned().collect_vec();
    let to_heads = to_repo.view().heads().iter().cloned().collect_vec();

    // Find newly added commits in `to_repo` which were not present in
    // `from_repo`.
    for commit in revset::walk_revs(repo, &to_heads, &from_heads)
        .unwrap()
        .iter()
        .commits(repo.store())
    {
        let commit = commit?;
        let modified_change = changes
            .entry(commit.change_id().clone())
            .or_insert_with(|| ModifiedChange {
                added_commits: vec![],
                removed_commits: vec![],
            });
        modified_change.added_commits.push(commit);
    }

    // Find commits which were hidden in `to_repo`.
    for commit in revset::walk_revs(repo, &from_heads, &to_heads)
        .unwrap()
        .iter()
        .commits(repo.store())
    {
        let commit = commit?;
        let modified_change = changes
            .entry(commit.change_id().clone())
            .or_insert_with(|| ModifiedChange {
                added_commits: vec![],
                removed_commits: vec![],
            });
        modified_change.removed_commits.push(commit);
    }

    Ok(changes)
}

// Displays the diffs of a modified change. The output differs based on the
// commits added and removed for the change.
// If there is a single added and removed commit, the diff is shown between the
// removed commit and the added commit rebased onto the removed commit's
// parents. If there is only a single added or single removed commit, the diff
// is shown of that commit's contents.
fn show_change_diff(
    ui: &Ui,
    formatter: &mut dyn Formatter,
    tx: &WorkspaceCommandTransaction,
    diff_renderer: &DiffRenderer,
    modified_change: &ModifiedChange,
) -> Result<(), CommandError> {
    if modified_change.added_commits.len() == 1 && modified_change.removed_commits.len() == 1 {
        let commit = &modified_change.added_commits[0];
        let predecessor = &modified_change.removed_commits[0];
        let predecessor_tree = rebase_to_dest_parent(tx.repo(), predecessor, commit)?;
        let tree = commit.tree()?;
        diff_renderer.show_diff(ui, formatter, &predecessor_tree, &tree, &EverythingMatcher)?;
    } else if modified_change.added_commits.len() == 1 {
        let commit = &modified_change.added_commits[0];
        diff_renderer.show_patch(ui, formatter, commit, &EverythingMatcher)?;
    } else if modified_change.removed_commits.len() == 1 {
        let commit = &modified_change.removed_commits[0];
        diff_renderer.show_patch(ui, formatter, commit, &EverythingMatcher)?;
    }

    Ok(())
}
