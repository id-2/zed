use std::{
    any::{Any, TypeId},
    cmp::Ordering,
    collections::HashSet,
    ops::Range,
    path::Path,
    time::Duration,
};

use anyhow::Context as _;
use collections::{BTreeMap, HashMap};
use feature_flags::FeatureFlagAppExt;
use futures::{stream::FuturesUnordered, StreamExt};
use git::{diff::DiffHunk, repository::GitFileStatus};
use gpui::{
    actions, AnyElement, AnyView, AppContext, EventEmitter, FocusHandle, FocusableView,
    InteractiveElement, Model, Render, SemanticVersion, Subscription, Task, View, WeakView,
};
use language::{Buffer, BufferRow, BufferSnapshot};
use multi_buffer::{ExcerptId, ExcerptRange, ExpandExcerptDirection, MultiBuffer};
use project::{Project, ProjectEntryId, ProjectPath, WorktreeId};
use settings::SettingsStore;
use text::{OffsetRangeExt, ToPoint};
use theme::ActiveTheme;
use ui::{
    div, h_flex, Color, Context, FluentBuilder, Icon, IconName, IntoElement, Label, LabelCommon,
    ParentElement, SharedString, Styled, ViewContext, VisualContext, WindowContext,
};
use util::{paths::compare_paths, ResultExt};
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle, TabContentParams},
    ItemNavHistory, ToolbarItemLocation, Workspace,
};

use crate::{Editor, EditorEvent, DEFAULT_MULTIBUFFER_CONTEXT};

actions!(project_diff, [Deploy]);

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(ProjectDiffEditor::register).detach();
}

const UPDATE_DEBOUNCE: Duration = Duration::from_millis(50);

struct ProjectDiffEditor {
    buffer_changes: BTreeMap<WorktreeId, HashMap<ProjectEntryId, Changes>>,
    entry_order: HashMap<WorktreeId, Vec<(ProjectPath, ProjectEntryId)>>,
    excerpts: Model<MultiBuffer>,
    editor: View<Editor>,

    project: Model<Project>,
    workspace: WeakView<Workspace>,
    focus_handle: FocusHandle,
    worktree_rescans: HashMap<WorktreeId, Task<()>>,
    _subscriptions: Vec<Subscription>,
}

struct Changes {
    status: GitFileStatus,
    buffer: Model<Buffer>,
    hunks: Vec<DiffHunk>,
}

impl ProjectDiffEditor {
    fn register(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) {
        if cx.is_staff() {
            workspace.register_action(Self::deploy);
        }
    }

    fn deploy(workspace: &mut Workspace, _: &Deploy, cx: &mut ViewContext<Workspace>) {
        if let Some(existing) = workspace.item_of_type::<Self>(cx) {
            workspace.activate_item(&existing, true, true, cx);
        } else {
            let workspace_handle = cx.view().downgrade();
            let project_diff =
                cx.new_view(|cx| Self::new(workspace.project().clone(), workspace_handle, cx));
            workspace.add_item_to_active_pane(Box::new(project_diff), None, true, cx);
        }
    }

    fn new(
        project: Model<Project>,
        workspace: WeakView<Workspace>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        // TODO kb diff change subscriptions. For that, needed:
        // * `-20/+50` stats retrieval: some background process that reacts on file changes
        let focus_handle = cx.focus_handle();
        let changed_entries_subscription =
            cx.subscribe(&project, |project_diff_editor, _, e, cx| {
                let mut worktree_to_rescan = None;
                match e {
                    project::Event::WorktreeAdded(id) => {
                        worktree_to_rescan = Some(*id);
                        // project_diff_editor
                        //     .buffer_changes
                        //     .insert(*id, HashMap::default());
                    }
                    project::Event::WorktreeRemoved(id) => {
                        project_diff_editor.buffer_changes.remove(id);
                    }
                    project::Event::WorktreeUpdatedEntries(id, updated_entries) => {
                        // TODO kb cannot invalidate buffer entries without invalidating the corresponding excerpts and order entries.
                        worktree_to_rescan = Some(*id);
                        // let entry_changes =
                        //     project_diff_editor.buffer_changes.entry(*id).or_default();
                        // for (_, entry_id, change) in updated_entries.iter() {
                        //     let changes = entry_changes.entry(*entry_id);
                        //     match change {
                        //         project::PathChange::Removed => {
                        //             if let hash_map::Entry::Occupied(entry) = changes {
                        //                 entry.remove();
                        //             }
                        //         }
                        //         // TODO kb understand the invalidation case better: now, we do that but still rescan the entire worktree
                        //         // What if we already have the buffer loaded inside the diff multi buffer and it was edited there? We should not do anything.
                        //         _ => match changes {
                        //             hash_map::Entry::Occupied(mut o) => o.get_mut().invalidate(),
                        //             hash_map::Entry::Vacant(v) => {
                        //                 v.insert(None);
                        //             }
                        //         },
                        //     }
                        // }
                    }
                    project::Event::WorktreeUpdatedGitRepositories(id) => {
                        worktree_to_rescan = Some(*id);
                        // project_diff_editor.buffer_changes.clear();
                    }
                    project::Event::DeletedEntry(id, entry_id) => {
                        worktree_to_rescan = Some(*id);
                        // if let Some(entries) = project_diff_editor.buffer_changes.get_mut(id) {
                        //     entries.remove(entry_id);
                        // }
                    }
                    project::Event::Closed => {
                        project_diff_editor.buffer_changes.clear();
                    }
                    _ => {}
                }

                if let Some(worktree_to_rescan) = worktree_to_rescan {
                    project_diff_editor.schedule_worktree_rescan(worktree_to_rescan, cx);
                }
            });

        let excerpts = cx.new_model(|cx| MultiBuffer::new(project.read(cx).capability()));

        let editor = cx.new_view(|cx| {
            let mut diff_display_editor =
                Editor::for_multibuffer(excerpts.clone(), Some(project.clone()), true, cx);
            diff_display_editor.set_expand_all_diff_hunks();
            diff_display_editor
        });

        let mut new_self = Self {
            project,
            workspace,
            buffer_changes: BTreeMap::default(),
            entry_order: HashMap::default(),
            worktree_rescans: HashMap::default(),
            focus_handle,
            editor,
            excerpts,
            _subscriptions: vec![changed_entries_subscription],
        };
        new_self.schedule_rescan_all(cx);
        new_self
    }

    fn schedule_rescan_all(&mut self, cx: &mut ViewContext<Self>) {
        let mut current_worktrees = HashSet::<WorktreeId>::default();
        for worktree in self.project.read(cx).worktrees(cx).collect::<Vec<_>>() {
            let worktree_id = worktree.read(cx).id();
            current_worktrees.insert(worktree_id);
            self.schedule_worktree_rescan(worktree_id, cx);
        }

        self.worktree_rescans
            .retain(|worktree_id, _| current_worktrees.contains(worktree_id));
        self.buffer_changes
            .retain(|worktree_id, _| current_worktrees.contains(worktree_id));
        self.entry_order
            .retain(|worktree_id, _| current_worktrees.contains(worktree_id));
    }

    fn schedule_worktree_rescan(&mut self, id: WorktreeId, cx: &mut ViewContext<Self>) {
        let project = self.project.clone();
        self.worktree_rescans.insert(
            id,
            cx.spawn(|project_diff_editor, mut cx| async move {
                cx.background_executor().timer(UPDATE_DEBOUNCE).await;
                let open_tasks = project
                    .update(&mut cx, |project, cx| {
                        let worktree = project.worktree_for_id(id, cx)?;
                        let applicable_entries = worktree
                            .read(cx)
                            .entries(false, 0)
                            .filter(|entry| !entry.is_external)
                            .filter(|entry| entry.is_file())
                            .filter_map(|entry| Some((entry.git_status?, entry)))
                            .filter_map(|(git_status, entry)| {
                                Some((git_status, entry.id, project.path_for_entry(entry.id, cx)?))
                            })
                            .collect::<Vec<_>>();
                        Some(
                            applicable_entries
                                .into_iter()
                                .map(|(status, entry_id, entry_path)| {
                                    let open_task = project.open_path(entry_path.clone(), cx);
                                    (status, entry_id, entry_path, open_task)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let buffers_with_git_diff = cx
                    .background_executor()
                    .spawn(async move {
                        let mut open_tasks = open_tasks
                            .into_iter()
                            .map(|(status, entry_id, entry_path, open_task)| async move {
                                let (_, opened_model) = open_task.await.with_context(|| {
                                    format!(
                                        "loading buffer {} for git diff",
                                        entry_path.path.display()
                                    )
                                })?;
                                let buffer = match opened_model.downcast::<Buffer>() {
                                    Ok(buffer) => buffer,
                                    Err(_model) => anyhow::bail!(
                                        "Could not load {} as a buffer for git diff",
                                        entry_path.path.display()
                                    ),
                                };
                                anyhow::Ok((status, entry_id, entry_path, buffer))
                            })
                            .collect::<FuturesUnordered<_>>();

                        let mut buffers_with_git_diff = Vec::new();
                        while let Some(opened_buffer) = open_tasks.next().await {
                            if let Some(opened_buffer) = opened_buffer.log_err() {
                                buffers_with_git_diff.push(opened_buffer);
                            }
                        }
                        buffers_with_git_diff
                    })
                    .await;

                let Some((buffers, mut new_entries)) = cx
                    .update(|cx| {
                        let mut buffers = HashMap::<
                            ProjectEntryId,
                            (GitFileStatus, Model<Buffer>, BufferSnapshot),
                        >::default();
                        let mut new_entries = Vec::new();
                        for (status, entry_id, entry_path, buffer) in buffers_with_git_diff {
                            let buffer_snapshot = buffer.read(cx).snapshot();
                            buffers.insert(entry_id, (status, buffer, buffer_snapshot));
                            new_entries.push((entry_path, entry_id));
                        }
                        (buffers, new_entries)
                    })
                    .ok()
                else {
                    return;
                };

                let (new_changes, new_entry_order) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut new_changes = HashMap::<ProjectEntryId, Changes>::default();
                        for (entry_id, (status, buffer, buffer_snapshot)) in buffers {
                            new_changes.insert(
                                entry_id,
                                Changes {
                                    status,
                                    buffer,
                                    hunks: buffer_snapshot
                                        .git_diff_hunks_in_row_range(0..BufferRow::MAX)
                                        .collect::<Vec<_>>(),
                                },
                            );
                        }

                        new_entries.sort_by(|(project_path_a, _), (project_path_b, _)| {
                            compare_paths(
                                (project_path_a.path.as_ref(), true),
                                (project_path_b.path.as_ref(), true),
                            )
                        });
                        (new_changes, new_entries)
                    })
                    .await;

                let mut diff_recalculations = FuturesUnordered::new();
                project_diff_editor
                    .update(&mut cx, |project_diff_editor, cx| {
                        project_diff_editor.update_excerpts(id, new_changes, new_entry_order, cx);
                        for buffer in project_diff_editor
                            .editor
                            .read(cx)
                            .buffer()
                            .read(cx)
                            .all_buffers()
                        {
                            buffer.update(cx, |buffer, cx| {
                                if let Some(diff_recalculation) = buffer.recalculate_diff(cx) {
                                    diff_recalculations.push(diff_recalculation);
                                }
                            });
                        }
                    })
                    .ok();

                cx.background_executor()
                    .spawn(async move {
                        while let Some(()) = diff_recalculations.next().await {
                            // another diff is calculated
                        }
                    })
                    .await;
            }),
        );
    }

    fn update_excerpts(
        &mut self,
        worktree_id: WorktreeId,
        new_changes: HashMap<ProjectEntryId, Changes>,
        new_entry_order: Vec<(ProjectPath, ProjectEntryId)>,
        cx: &mut ViewContext<ProjectDiffEditor>,
    ) {
        if let Some(current_order) = self.entry_order.get(&worktree_id) {
            let current_entries = self.buffer_changes.entry(worktree_id).or_default();
            let mut new_order_entries = new_entry_order.iter().fuse().peekable();
            let mut excerpts_to_remove = Vec::new();
            let mut new_excerpt_hunks = BTreeMap::<
                ExcerptId,
                Vec<(ProjectPath, Model<Buffer>, Vec<Range<text::Anchor>>)>,
            >::new();
            let mut excerpt_to_expand =
                HashMap::<(u32, ExpandExcerptDirection), Vec<ExcerptId>>::default();
            let mut latest_excerpt_id = ExcerptId::min();

            for (current_path, current_entry_id) in current_order {
                let current_changes = match current_entries.get(current_entry_id) {
                    Some(current_changes) => {
                        if current_changes.hunks.is_empty() {
                            continue;
                        }
                        current_changes
                    }
                    None => continue,
                };
                let buffer_excerpts = self
                    .excerpts
                    .read(cx)
                    .excerpts_for_buffer(&current_changes.buffer, cx);
                let last_current_excerpt_id =
                    buffer_excerpts.last().map(|(excerpt_id, _)| *excerpt_id);
                let mut current_excerpts = buffer_excerpts.into_iter().fuse().peekable();
                loop {
                    match new_order_entries.peek() {
                        Some((new_path, new_entry)) => {
                            match compare_paths(
                                (current_path.path.as_ref(), true),
                                (new_path.path.as_ref(), true),
                            ) {
                                Ordering::Less => {
                                    excerpts_to_remove
                                        .extend(current_excerpts.map(|(excerpt_id, _)| excerpt_id));
                                    break;
                                }
                                Ordering::Greater => {
                                    if let Some(new_changes) = new_changes.get(new_entry) {
                                        if !new_changes.hunks.is_empty() {
                                            let hunks = new_excerpt_hunks
                                                .entry(latest_excerpt_id)
                                                .or_default();
                                            match hunks.binary_search_by(|(probe, ..)| {
                                                compare_paths(
                                                    (new_path.path.as_ref(), true),
                                                    (probe.path.as_ref(), true),
                                                )
                                            }) {
                                                Ok(i) => hunks[i].2.extend(
                                                    new_changes
                                                        .hunks
                                                        .iter()
                                                        .map(|hunk| hunk.buffer_range.clone()),
                                                ),
                                                Err(i) => hunks.insert(
                                                    i,
                                                    (
                                                        new_path.clone(),
                                                        new_changes.buffer.clone(),
                                                        new_changes
                                                            .hunks
                                                            .iter()
                                                            .map(|hunk| hunk.buffer_range.clone())
                                                            .collect(),
                                                    ),
                                                ),
                                            }
                                        }
                                    };
                                    let _ = new_order_entries.next();
                                }
                                Ordering::Equal => {
                                    match new_changes.get(new_entry) {
                                        Some(new_changes) => {
                                            let buffer_snapshot =
                                                new_changes.buffer.read(cx).snapshot();
                                            let mut current_hunks =
                                                current_changes.hunks.iter().fuse().peekable();
                                            let mut new_hunks_unchanged =
                                                Vec::with_capacity(new_changes.hunks.len());
                                            let mut new_hunks_with_updates =
                                                Vec::with_capacity(new_changes.hunks.len());
                                            'new_changes: for new_hunk in &new_changes.hunks {
                                                loop {
                                                    match current_hunks.peek() {
                                                        Some(current_hunk) => {
                                                            match (
                                                                current_hunk
                                                                    .buffer_range
                                                                    .start
                                                                    .cmp(
                                                                        &new_hunk
                                                                            .buffer_range
                                                                            .start,
                                                                        &buffer_snapshot,
                                                                    ),
                                                                current_hunk.buffer_range.end.cmp(
                                                                    &new_hunk.buffer_range.end,
                                                                    &buffer_snapshot,
                                                                ),
                                                            ) {
                                                                (
                                                                    Ordering::Equal,
                                                                    Ordering::Equal,
                                                                ) => {
                                                                    new_hunks_unchanged
                                                                        .push(new_hunk);
                                                                    let _ = current_hunks.next();
                                                                    continue 'new_changes;
                                                                }
                                                                (Ordering::Equal, _)
                                                                | (_, Ordering::Equal) => {
                                                                    new_hunks_with_updates
                                                                        .push(new_hunk);
                                                                    continue 'new_changes;
                                                                }
                                                                (
                                                                    Ordering::Less,
                                                                    Ordering::Greater,
                                                                )
                                                                | (
                                                                    Ordering::Greater,
                                                                    Ordering::Less,
                                                                ) => {
                                                                    new_hunks_with_updates
                                                                        .push(new_hunk);
                                                                    continue 'new_changes;
                                                                }
                                                                (
                                                                    Ordering::Less,
                                                                    Ordering::Less,
                                                                ) => {
                                                                    if current_hunk
                                                                        .buffer_range
                                                                        .start
                                                                        .cmp(
                                                                            &new_hunk
                                                                                .buffer_range
                                                                                .end,
                                                                            &buffer_snapshot,
                                                                        )
                                                                        .is_le()
                                                                    {
                                                                        new_hunks_with_updates
                                                                            .push(new_hunk);
                                                                        continue 'new_changes;
                                                                    } else {
                                                                        let _ =
                                                                            current_hunks.next();
                                                                    }
                                                                }
                                                                (
                                                                    Ordering::Greater,
                                                                    Ordering::Greater,
                                                                ) => {
                                                                    if current_hunk
                                                                        .buffer_range
                                                                        .end
                                                                        .cmp(
                                                                            &new_hunk
                                                                                .buffer_range
                                                                                .start,
                                                                            &buffer_snapshot,
                                                                        )
                                                                        .is_ge()
                                                                    {
                                                                        new_hunks_with_updates
                                                                            .push(new_hunk);
                                                                        continue 'new_changes;
                                                                    } else {
                                                                        let _ =
                                                                            current_hunks.next();
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        None => {
                                                            new_hunks_with_updates.push(new_hunk);
                                                            continue 'new_changes;
                                                        }
                                                    }
                                                }
                                            }

                                            let mut excerpts_with_new_changes =
                                                HashSet::<ExcerptId>::default();
                                            'new_hunks: for new_hunk in new_hunks_with_updates {
                                                loop {
                                                    match current_excerpts.peek() {
                                                        Some((
                                                            current_excerpt_id,
                                                            current_excerpt_range,
                                                        )) => {
                                                            match (
                                                                current_excerpt_range
                                                                    .context
                                                                    .start
                                                                    .cmp(
                                                                        &new_hunk
                                                                            .buffer_range
                                                                            .start,
                                                                        &buffer_snapshot,
                                                                    ),
                                                                current_excerpt_range
                                                                    .context
                                                                    .end
                                                                    .cmp(
                                                                        &new_hunk.buffer_range.end,
                                                                        &buffer_snapshot,
                                                                    ),
                                                            ) {
                                                                (
                                                                    Ordering::Less
                                                                    | Ordering::Equal,
                                                                    Ordering::Greater
                                                                    | Ordering::Equal,
                                                                ) => {
                                                                    excerpts_with_new_changes
                                                                        .insert(
                                                                            *current_excerpt_id,
                                                                        );
                                                                    continue 'new_hunks;
                                                                }
                                                                (
                                                                    Ordering::Greater
                                                                    | Ordering::Equal,
                                                                    Ordering::Less
                                                                    | Ordering::Equal,
                                                                ) => {
                                                                    let expand_up = current_excerpt_range
                                                                .context
                                                                .start
                                                                .to_point(&buffer_snapshot)
                                                                .row
                                                                .saturating_sub(
                                                                    new_hunk
                                                                        .buffer_range
                                                                        .start
                                                                        .to_point(&buffer_snapshot)
                                                                        .row,
                                                                );
                                                                    let expand_down = new_hunk
                                                                    .buffer_range
                                                                    .end
                                                                    .to_point(&buffer_snapshot)
                                                                    .row
                                                                    .saturating_sub(
                                                                        current_excerpt_range
                                                                            .context
                                                                            .end
                                                                            .to_point(
                                                                                &buffer_snapshot,
                                                                            )
                                                                            .row,
                                                                    );
                                                                    excerpt_to_expand.entry((expand_up.max(expand_down).max(DEFAULT_MULTIBUFFER_CONTEXT), ExpandExcerptDirection::UpAndDown)).or_default().push(*current_excerpt_id);
                                                                    excerpts_with_new_changes
                                                                        .insert(
                                                                            *current_excerpt_id,
                                                                        );
                                                                    continue 'new_hunks;
                                                                }
                                                                (
                                                                    Ordering::Less,
                                                                    Ordering::Less,
                                                                ) => {
                                                                    if current_excerpt_range
                                                                        .context
                                                                        .start
                                                                        .cmp(
                                                                            &new_hunk
                                                                                .buffer_range
                                                                                .end,
                                                                            &buffer_snapshot,
                                                                        )
                                                                        .is_le()
                                                                    {
                                                                        let expand_up = current_excerpt_range
                                                                        .context
                                                                        .start
                                                                        .to_point(&buffer_snapshot)
                                                                        .row
                                                                        .saturating_sub(
                                                                            new_hunk.buffer_range
                                                                                .start
                                                                                .to_point(
                                                                                    &buffer_snapshot,
                                                                                )
                                                                                .row,
                                                                        );
                                                                        excerpt_to_expand.entry((expand_up.max(DEFAULT_MULTIBUFFER_CONTEXT), ExpandExcerptDirection::Up)).or_default().push(*current_excerpt_id);
                                                                        excerpts_with_new_changes
                                                                            .insert(
                                                                                *current_excerpt_id,
                                                                            );
                                                                        continue 'new_hunks;
                                                                    } else {
                                                                        if !new_changes
                                                                            .hunks
                                                                            .is_empty()
                                                                        {
                                                                            let hunks = new_excerpt_hunks
                                                                                .entry(latest_excerpt_id)
                                                                                .or_default();
                                                                            match hunks.binary_search_by(|(probe, ..)| {
                                                                                compare_paths(
                                                                                    (new_path.path.as_ref(), true),
                                                                                    (probe.path.as_ref(), true),
                                                                                )
                                                                            }) {
                                                                                Ok(i) => hunks[i].2.extend(
                                                                                    new_changes
                                                                                        .hunks
                                                                                        .iter()
                                                                                        .map(|hunk| hunk.buffer_range.clone()),
                                                                                ),
                                                                                Err(i) => hunks.insert(
                                                                                    i,
                                                                                    (
                                                                                        new_path.clone(),
                                                                                        new_changes.buffer.clone(),
                                                                                        new_changes
                                                                                            .hunks
                                                                                            .iter()
                                                                                            .map(|hunk| hunk.buffer_range.clone())
                                                                                            .collect(),
                                                                                    ),
                                                                                ),
                                                                            }
                                                                        }
                                                                        continue 'new_hunks;
                                                                    }
                                                                }
                                                                /* TODO kb remove or leave?
                                                                    [    ><<<<<<<<new_e
                                                                ----[---->--]----<--
                                                                   cur_s > cur_e <
                                                                         >       <
                                                                    new_s>>>>>>>><
                                                                */
                                                                (
                                                                    Ordering::Greater,
                                                                    Ordering::Greater,
                                                                ) => {
                                                                    if current_excerpt_range
                                                                        .context
                                                                        .end
                                                                        .cmp(
                                                                            &new_hunk
                                                                                .buffer_range
                                                                                .start,
                                                                            &buffer_snapshot,
                                                                        )
                                                                        .is_ge()
                                                                    {
                                                                        let expand_down = new_hunk
                                                                    .buffer_range
                                                                    .end
                                                                    .to_point(&buffer_snapshot)
                                                                    .row
                                                                    .saturating_sub(
                                                                        current_excerpt_range
                                                                            .context
                                                                            .end
                                                                            .to_point(
                                                                                &buffer_snapshot,
                                                                            )
                                                                            .row,
                                                                    );
                                                                        excerpt_to_expand.entry((expand_down.max(DEFAULT_MULTIBUFFER_CONTEXT), ExpandExcerptDirection::Down)).or_default().push(*current_excerpt_id);
                                                                        excerpts_with_new_changes
                                                                            .insert(
                                                                                *current_excerpt_id,
                                                                            );
                                                                        continue 'new_hunks;
                                                                    } else {
                                                                        latest_excerpt_id =
                                                                            *current_excerpt_id;
                                                                        let _ =
                                                                            current_excerpts.next();
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        None => {
                                                            let hunks = new_excerpt_hunks
                                                                .entry(latest_excerpt_id)
                                                                .or_default();
                                                            match hunks.binary_search_by(
                                                                |(probe, ..)| {
                                                                    compare_paths(
                                                                        (
                                                                            new_path.path.as_ref(),
                                                                            true,
                                                                        ),
                                                                        (probe.path.as_ref(), true),
                                                                    )
                                                                },
                                                            ) {
                                                                Ok(i) => hunks[i].2.extend(
                                                                    new_changes.hunks.iter().map(
                                                                        |hunk| {
                                                                            hunk.buffer_range
                                                                                .clone()
                                                                        },
                                                                    ),
                                                                ),
                                                                Err(i) => hunks.insert(
                                                                    i,
                                                                    (
                                                                        new_path.clone(),
                                                                        new_changes.buffer.clone(),
                                                                        new_changes
                                                                            .hunks
                                                                            .iter()
                                                                            .map(|hunk| {
                                                                                hunk.buffer_range
                                                                                    .clone()
                                                                            })
                                                                            .collect(),
                                                                    ),
                                                                ),
                                                            }
                                                            continue 'new_hunks;
                                                        }
                                                    }
                                                }
                                            }

                                            for (excerpt_id, excerpt_range) in current_excerpts {
                                                if !excerpts_with_new_changes.contains(&excerpt_id)
                                                    && !new_hunks_unchanged.iter().any(|hunk| {
                                                        excerpt_range
                                                            .context
                                                            .start
                                                            .cmp(
                                                                &hunk.buffer_range.end,
                                                                &buffer_snapshot,
                                                            )
                                                            .is_le()
                                                            && excerpt_range
                                                                .context
                                                                .end
                                                                .cmp(
                                                                    &hunk.buffer_range.start,
                                                                    &buffer_snapshot,
                                                                )
                                                                .is_ge()
                                                    })
                                                {
                                                    excerpts_to_remove.push(excerpt_id);
                                                }
                                                latest_excerpt_id = excerpt_id;
                                            }
                                        }
                                        None => excerpts_to_remove.extend(
                                            current_excerpts.map(|(excerpt_id, _)| excerpt_id),
                                        ),
                                    }
                                    let _ = new_order_entries.next();
                                    break;
                                }
                            }
                        }
                        None => {
                            excerpts_to_remove
                                .extend(current_excerpts.map(|(excerpt_id, _)| excerpt_id));
                            break;
                        }
                    }
                }
                latest_excerpt_id = last_current_excerpt_id.unwrap_or(latest_excerpt_id);
            }

            for (path, project_entry_id) in new_order_entries {
                if let Some(changes) = new_changes.get(project_entry_id) {
                    if !changes.hunks.is_empty() {
                        let hunks = new_excerpt_hunks.entry(latest_excerpt_id).or_default();
                        match hunks.binary_search_by(|(probe, ..)| {
                            compare_paths((path.path.as_ref(), true), (probe.path.as_ref(), true))
                        }) {
                            Ok(i) => hunks[i]
                                .2
                                .extend(changes.hunks.iter().map(|hunk| hunk.buffer_range.clone())),
                            Err(i) => hunks.insert(
                                i,
                                (
                                    path.clone(),
                                    changes.buffer.clone(),
                                    changes
                                        .hunks
                                        .iter()
                                        .map(|hunk| hunk.buffer_range.clone())
                                        .collect(),
                                ),
                            ),
                        }
                    }
                }
            }

            self.excerpts.update(cx, |multi_buffer, cx| {
                for (mut after_excerpt_id, excerpts_to_add) in new_excerpt_hunks {
                    for (_, buffer, hunk_ranges) in excerpts_to_add {
                        let buffer_snapshot = buffer.read(cx).snapshot();
                        let max_point = buffer_snapshot.max_point();
                        let new_excerpts = multi_buffer.insert_excerpts_after(
                            after_excerpt_id,
                            buffer,
                            hunk_ranges.into_iter().map(|range| {
                                let mut extended_point_range = range.to_point(&buffer_snapshot);
                                extended_point_range.start.row = extended_point_range
                                    .start
                                    .row
                                    .saturating_sub(DEFAULT_MULTIBUFFER_CONTEXT);
                                extended_point_range.end.row = (extended_point_range.end.row
                                    + DEFAULT_MULTIBUFFER_CONTEXT)
                                    .min(max_point.row);
                                ExcerptRange {
                                    context: extended_point_range,
                                    primary: None,
                                }
                            }),
                            cx,
                        );
                        after_excerpt_id = new_excerpts.last().copied().unwrap_or(after_excerpt_id);
                    }
                }
                multi_buffer.remove_excerpts(excerpts_to_remove, cx);
                for ((line_count, direction), excerpts) in excerpt_to_expand {
                    multi_buffer.expand_excerpts(excerpts, line_count, direction, cx);
                }
            });
        } else {
            self.excerpts.update(cx, |multi_buffer, cx| {
                for new_changes in new_entry_order
                    .iter()
                    .filter_map(|(_, entry_id)| new_changes.get(entry_id))
                {
                    multi_buffer.push_excerpts_with_context_lines(
                        new_changes.buffer.clone(),
                        new_changes
                            .hunks
                            .iter()
                            .map(|hunk| hunk.buffer_range.clone())
                            .collect(),
                        DEFAULT_MULTIBUFFER_CONTEXT,
                        cx,
                    );
                }
            });
        };

        let mut new_changes = new_changes;
        let mut new_entry_order = new_entry_order;
        std::mem::swap(
            self.buffer_changes.entry(worktree_id).or_default(),
            &mut new_changes,
        );
        std::mem::swap(
            self.entry_order.entry(worktree_id).or_default(),
            &mut new_entry_order,
        );
    }
}

impl EventEmitter<EditorEvent> for ProjectDiffEditor {}

impl FocusableView for ProjectDiffEditor {
    fn focus_handle(&self, _: &AppContext) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ProjectDiffEditor {
    type Event = EditorEvent;

    fn to_item_events(event: &EditorEvent, f: impl FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn tab_tooltip_text(&self, _: &AppContext) -> Option<SharedString> {
        Some("Project Diff".into())
    }

    fn tab_content(&self, params: TabContentParams, _: &WindowContext) -> AnyElement {
        if self.buffer_changes.is_empty() {
            Label::new("No changes")
                .color(if params.selected {
                    Color::Default
                } else {
                    Color::Muted
                })
                .into_any_element()
        } else {
            h_flex()
                .gap_1()
                .when(true, |then| {
                    then.child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::XCircle).color(Color::Error))
                            .child(Label::new(self.buffer_changes.len().to_string()).color(
                                if params.selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )),
                    )
                })
                .when(true, |then| {
                    then.child(
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::Indicator).color(Color::Warning))
                            .child(Label::new(self.buffer_changes.len().to_string()).color(
                                if params.selected {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )),
                    )
                })
                .into_any_element()
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("project diagnostics")
    }

    fn for_each_project_item(
        &self,
        cx: &AppContext,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &AppContext) -> bool {
        false
    }

    fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut ViewContext<Self>) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        cx: &mut ViewContext<Self>,
    ) -> Option<View<Self>>
    where
        Self: Sized,
    {
        Some(cx.new_view(|cx| {
            ProjectDiffEditor::new(self.project.clone(), self.workspace.clone(), cx)
        }))
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.excerpts.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.excerpts.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _: &AppContext) -> bool {
        true
    }

    fn save(
        &mut self,
        format: bool,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.save(format, project, cx)
    }

    fn save_as(
        &mut self,
        _: Model<Project>,
        _: ProjectPath,
        _: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Model<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.editor.reload(project, cx)
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a View<Self>,
        _: &'a AppContext,
    ) -> Option<AnyView> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.to_any())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.to_any())
        } else {
            None
        }
    }

    fn breadcrumb_location(&self, _: &AppContext) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        self.editor.breadcrumbs(theme, cx)
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, cx: &mut ViewContext<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx));
    }
}

impl Render for ProjectDiffEditor {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let child = if self.buffer_changes.is_empty() {
            div()
                .bg(cx.theme().colors().editor_background)
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .child(Label::new("No changes in the workspace"))
        } else {
            div().size_full().child(self.editor.clone())
        };

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child(child)
    }
}

#[cfg(test)]
mod tests {
    use std::{ops::Deref as _, sync::Arc};

    use fs::RealFs;
    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    // #[gpui::test]
    // async fn randomized_tests(cx: &mut TestAppContext) {
    //     // Create a new project (how?? temp fs?),
    //     let fs = FakeFs::new(cx.executor());
    //     let project = Project::test(fs, [], cx).await;

    //     // create random files with random content

    //     // Commit it into git somehow (technically can do with "real" fs in a temp dir)
    //     //
    //     // Apply randomized changes to the project: select a random file, random change and apply to buffers
    // }

    #[gpui::test]
    async fn simple_edit_test(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path();

        std::fs::write(dst.join("file_a"), "This is file_a").unwrap();
        std::fs::write(dst.join("file_b"), "This is file_b").unwrap();

        run_git(dst, &["init"]);
        run_git(dst, &["add", "*"]);
        run_git(dst, &["commit", "-m", "Initial commit"]);

        let project = Project::test(Arc::new(RealFs::default()), [dst], cx).await;
        let workspace = cx.add_window(|cx| Workspace::test_new(project.clone(), cx));
        let cx = &mut VisualTestContext::from_window(*workspace.deref(), cx);

        let file_a_editor = workspace
            .update(cx, |workspace, cx| {
                let file_a_editor = workspace.open_abs_path(dst.join("file_a"), true, cx);
                ProjectDiffEditor::deploy(workspace, &Deploy, cx);
                file_a_editor
            })
            .unwrap()
            .await
            .expect("did not open an item at all")
            .downcast::<Editor>()
            .expect("did not open an editor for file_a");

        let project_diff_editor = workspace
            .update(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .items()
                    .find_map(|item| item.downcast::<ProjectDiffEditor>())
            })
            .unwrap()
            .expect("did not find a ProjectDiffEditor");
        project_diff_editor.update(cx, |project_diff_editor, cx| {
            assert!(
                project_diff_editor.editor.read(cx).text(cx).is_empty(),
                "Should have no changes after opening the diff on no git changes"
            );
        });

        let old_text = file_a_editor.update(cx, |editor, cx| editor.text(cx));
        let change = "an edit after git add";
        let save_task = file_a_editor
            .update(cx, |file_a_editor, cx| {
                file_a_editor.insert(change, cx);
                file_a_editor.save(false, project.clone(), cx)
            })
            .await
            .expect("failed to save a file");
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();

        project_diff_editor.update(cx, |project_diff_editor, cx| {
            // TODO kb assert it better: extract added text (based on the background changes) and deleted text (based on the deleted blocks added)
            assert_eq!(
                project_diff_editor.editor.read(cx).text(cx),
                format!("{change}{old_text}"),
                "Should have a new change shown in the beginning, and the old text shown as deleted text afterwards"
            );
        });
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("git commit failed");

        format!(
            "Stdout: {}; stderr: {}",
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        )
    }

    fn init_test(cx: &mut gpui::TestAppContext) {
        if std::env::var("RUST_LOG").is_ok() {
            env_logger::try_init().ok();
        }

        cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(SemanticVersion::default(), cx);
            client::init_settings(cx);
            language::init(cx);
            Project::init_settings(cx);
            workspace::init_settings(cx);
            crate::init(cx);
        });
    }
}
