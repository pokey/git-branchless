//! Display a graph of commits that the user has worked on recently.
//!
//! The set of commits that are still being worked on is inferred from the event
//! log; see the `eventlog` module.

use std::cmp::Ordering;
use std::fmt::Write;
use std::mem::swap;
use std::time::SystemTime;

use console::style;
use eden_dag::DagAlgorithm;
use lib::core::config::{get_hint_enabled, print_hint_suppression_notice, Hint};
use lib::core::repo_ext::RepoExt;
use lib::core::rewrite::find_rewrite_target;
use lib::util::ExitCode;
use tracing::instrument;

use lib::core::dag::{CommitSet, Dag};
use lib::core::effects::Effects;
use lib::core::eventlog::{EventLogDb, EventReplayer};
use lib::core::formatting::{printable_styled_string, Pluralize};
use lib::core::node_descriptors::{
    BranchesDescriptor, CommitMessageDescriptor, CommitOidDescriptor,
    DifferentialRevisionDescriptor, ObsolescenceExplanationDescriptor, Redactor,
    RelativeTimeDescriptor,
};
use lib::git::{GitRunInfo, Repo};

pub use graph::{make_smartlog_graph, SmartlogGraph};
pub use render::{render_graph, SmartlogOptions};

use crate::revset::resolve_commits;

mod graph {
    use std::collections::HashMap;
    use std::convert::TryFrom;

    use eden_dag::DagAlgorithm;
    use lib::core::gc::mark_commit_reachable;
    use tracing::instrument;

    use lib::core::dag::{commit_set_to_vec_unsorted, CommitSet, Dag};
    use lib::core::effects::{Effects, OperationType};
    use lib::core::eventlog::{EventCursor, EventReplayer};
    use lib::core::node_descriptors::NodeObject;
    use lib::git::{Commit, Time};
    use lib::git::{NonZeroOid, Repo};

    /// Node contained in the smartlog commit graph.
    #[derive(Debug)]
    pub struct Node<'repo> {
        /// The underlying commit object.
        pub object: NodeObject<'repo>,

        /// The OID of the parent node in the smartlog commit graph.
        ///
        /// This is different from inspecting `commit.parents()`,& since the smartlog
        /// will hide most nodes from the commit graph, including parent nodes.
        pub parent: Option<NonZeroOid>,

        /// The OIDs of the children nodes in the smartlog commit graph.
        pub children: Vec<NonZeroOid>,

        /// Indicates that this is a commit to the main branch.
        ///
        /// These commits are considered to be immutable and should never leave the
        /// `main` state. But this can still happen in practice if the user's
        /// workflow is different than expected.
        pub is_main: bool,

        /// Indicates that this commit has been marked as obsolete.
        ///
        /// Commits are marked as obsolete when they've been rewritten into another
        /// commit, or explicitly marked such by the user. Normally, they're not
        /// visible in the smartlog, except if there's some anomalous situation that
        /// the user should take note of (such as an obsolete commit having a
        /// non-obsolete descendant).
        ///
        /// Occasionally, a main commit can be marked as obsolete, such as if a
        /// commit in the main branch has been rewritten. We don't expect this to
        /// happen in the monorepo workflow, but it can happen in other workflows
        /// where you commit directly to the main branch and then later rewrite the
        /// commit.
        pub is_obsolete: bool,
    }

    /// Graph of commits that the user is working on.
    pub struct SmartlogGraph<'repo> {
        pub nodes: HashMap<NonZeroOid, Node<'repo>>,
    }

    impl<'repo> SmartlogGraph<'repo> {
        /// Get a list of commits stored in the graph.
        /// Returns commits in descending commit time order.
        pub fn get_commits(&self) -> Vec<Commit<'repo>> {
            let mut commits = self
                .nodes
                .values()
                .filter_map(|node| match &node.object {
                    NodeObject::Commit { commit } => Some(commit.clone()),
                    NodeObject::GarbageCollected { oid: _ } => None,
                })
                .collect::<Vec<Commit<'repo>>>();
            commits.sort_by_key(|commit| (commit.get_committer().get_time(), commit.get_oid()));
            commits.reverse();
            commits
        }
    }

    impl std::fmt::Debug for SmartlogGraph<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "<CommitGraph len={}>", self.nodes.len())
        }
    }

    /// Find additional commits that should be displayed.
    ///
    /// For example, if you check out a commit that has intermediate parent commits
    /// between it and the main branch, those intermediate commits should be shown
    /// (or else you won't get a good idea of the line of development that happened
    /// for this commit since the main branch).
    #[instrument]
    fn walk_from_active_heads<'repo>(
        effects: &Effects,
        repo: &'repo Repo,
        dag: &Dag,
        public_commits: &CommitSet,
        active_heads: &CommitSet,
    ) -> eyre::Result<SmartlogGraph<'repo>> {
        let mut graph: HashMap<NonZeroOid, Node> = {
            let mut result = HashMap::new();
            for vertex in active_heads.iter()? {
                let vertex = vertex?;
                let path_to_main_branch =
                    dag.find_path_to_main_branch(effects, CommitSet::from(vertex.clone()))?;
                let path_to_main_branch = match path_to_main_branch {
                    Some(path_to_main_branch) => path_to_main_branch,
                    None => CommitSet::from(vertex.clone()),
                };

                for vertex in path_to_main_branch.iter_rev()? {
                    let vertex = vertex?;
                    let oid = NonZeroOid::try_from(vertex.clone())?;

                    let object = match repo.find_commit(oid)? {
                        Some(commit) => NodeObject::Commit { commit },
                        None => {
                            // Assume that this commit was garbage collected.
                            NodeObject::GarbageCollected { oid }
                        }
                    };

                    result.insert(
                        oid,
                        Node {
                            object,
                            parent: None,         // populated below
                            children: Vec::new(), // populated below
                            is_main: public_commits.contains(&vertex)?,
                            is_obsolete: dag.obsolete_commits.contains(&vertex)?,
                        },
                    );
                }
            }
            result
        };

        // Find immediate parent-child links.
        let links: Vec<(NonZeroOid, NonZeroOid)> = {
            let non_main_node_oids =
                graph.iter().filter_map(
                    |(child_oid, node)| if !node.is_main { Some(child_oid) } else { None },
                );

            let mut links = Vec::new();
            for child_oid in non_main_node_oids {
                let parent_vertexes = dag.query().parents(CommitSet::from(*child_oid))?;
                let parent_oids = commit_set_to_vec_unsorted(&parent_vertexes)?;
                for parent_oid in parent_oids {
                    if graph.contains_key(&parent_oid) {
                        links.push((*child_oid, parent_oid))
                    }
                }
            }
            links
        };

        for (child_oid, parent_oid) in links.iter() {
            graph.get_mut(child_oid).unwrap().parent = Some(*parent_oid);
            graph.get_mut(parent_oid).unwrap().children.push(*child_oid);
        }

        Ok(SmartlogGraph { nodes: graph })
    }

    /// Sort children nodes of the commit graph in a standard order, for determinism
    /// in output.
    fn sort_children(graph: &mut SmartlogGraph) {
        let commit_times: HashMap<NonZeroOid, Option<Time>> = graph
            .nodes
            .iter()
            .map(|(oid, node)| {
                (
                    *oid,
                    match &node.object {
                        NodeObject::Commit { commit } => Some(commit.get_time()),
                        NodeObject::GarbageCollected { oid: _ } => None,
                    },
                )
            })
            .collect();
        for node in graph.nodes.values_mut() {
            node.children
                .sort_by_key(|child_oid| (&commit_times[child_oid], child_oid.to_string()));
        }
    }

    /// Construct the smartlog graph for the repo.
    #[instrument]
    pub fn make_smartlog_graph<'repo>(
        effects: &Effects,
        repo: &'repo Repo,
        dag: &Dag,
        event_replayer: &EventReplayer,
        event_cursor: EventCursor,
        observed_commits: &CommitSet,
        remove_commits: bool,
    ) -> eyre::Result<SmartlogGraph<'repo>> {
        let (effects, _progress) = effects.start_operation(OperationType::MakeGraph);

        let mut graph = {
            let (effects, _progress) = effects.start_operation(OperationType::WalkCommits);

            let public_commits = dag.query_public_commits()?;

            let observed_commits = if remove_commits {
                observed_commits.difference(&dag.obsolete_commits)
            } else {
                observed_commits.clone()
            };

            let active_heads = dag.query_active_heads(&public_commits, &observed_commits)?;
            for oid in commit_set_to_vec_unsorted(&active_heads)? {
                mark_commit_reachable(repo, oid)?;
            }

            walk_from_active_heads(&effects, repo, dag, &public_commits, &active_heads)?
        };
        sort_children(&mut graph);
        Ok(graph)
    }
}

mod render {
    use std::cmp::Ordering;

    use cursive::theme::Effect;
    use cursive::utils::markup::StyledString;
    use eden_dag::DagAlgorithm;
    use tracing::instrument;

    use lib::core::dag::{CommitSet, CommitVertex, Dag};
    use lib::core::effects::Effects;
    use lib::core::formatting::set_effect;
    use lib::core::formatting::{Glyphs, StyledStringBuilder};
    use lib::core::node_descriptors::{render_node_descriptors, NodeDescriptor};
    use lib::git::{NonZeroOid, Repo};

    use crate::opts::Revset;

    use super::graph::SmartlogGraph;

    /// Split fully-independent subgraphs into multiple graphs.
    ///
    /// This is intended to handle the situation of having multiple lines of work
    /// rooted from different commits in the main branch.
    ///
    /// Returns the list such that the topologically-earlier subgraphs are first in
    /// the list (i.e. those that would be rendered at the bottom of the smartlog).
    fn split_commit_graph_by_roots(
        effects: &Effects,
        repo: &Repo,
        dag: &Dag,
        graph: &SmartlogGraph,
    ) -> Vec<NonZeroOid> {
        let mut root_commit_oids: Vec<NonZeroOid> = graph
            .nodes
            .iter()
            .filter(|(_oid, node)| node.parent.is_none())
            .map(|(oid, _node)| oid)
            .copied()
            .collect();

        let compare = |lhs_oid: &NonZeroOid, rhs_oid: &NonZeroOid| -> Ordering {
            let lhs_commit = repo.find_commit(*lhs_oid);
            let rhs_commit = repo.find_commit(*rhs_oid);

            let (lhs_commit, rhs_commit) = match (lhs_commit, rhs_commit) {
                (Ok(Some(lhs_commit)), Ok(Some(rhs_commit))) => (lhs_commit, rhs_commit),
                _ => return lhs_oid.cmp(rhs_oid),
            };

            let merge_base_oid = dag.get_one_merge_base_oid(effects, repo, *lhs_oid, *rhs_oid);
            let merge_base_oid = match merge_base_oid {
                Err(_) => return lhs_oid.cmp(rhs_oid),
                Ok(merge_base_oid) => merge_base_oid,
            };

            match merge_base_oid {
                // lhs was topologically first, so it should be sorted earlier in the list.
                Some(merge_base_oid) if merge_base_oid == *lhs_oid => Ordering::Less,
                Some(merge_base_oid) if merge_base_oid == *rhs_oid => Ordering::Greater,

                // The commits were not orderable (pathlogical situation). Let's
                // just order them by timestamp in that case to produce a consistent
                // and reasonable guess at the intended topological ordering.
                Some(_) | None => match lhs_commit.get_time().cmp(&rhs_commit.get_time()) {
                    result @ Ordering::Less | result @ Ordering::Greater => result,
                    Ordering::Equal => lhs_oid.cmp(rhs_oid),
                },
            }
        };

        root_commit_oids.sort_by(compare);
        root_commit_oids
    }

    #[instrument(skip(commit_descriptors, graph))]
    fn get_child_output(
        glyphs: &Glyphs,
        graph: &SmartlogGraph,
        root_oids: &[NonZeroOid],
        commit_descriptors: &mut [&mut dyn NodeDescriptor],
        head_oid: Option<NonZeroOid>,
        current_oid: NonZeroOid,
        last_child_line_char: Option<&str>,
    ) -> eyre::Result<Vec<StyledString>> {
        let current_node = &graph.nodes[&current_oid];
        let is_head = Some(current_oid) == head_oid;

        let text = render_node_descriptors(glyphs, &current_node.object, commit_descriptors)?;
        let cursor = match (current_node.is_main, current_node.is_obsolete, is_head) {
            (false, false, false) => glyphs.commit_visible,
            (false, false, true) => glyphs.commit_visible_head,
            (false, true, false) => glyphs.commit_obsolete,
            (false, true, true) => glyphs.commit_obsolete_head,
            (true, false, false) => glyphs.commit_main,
            (true, false, true) => glyphs.commit_main_head,
            (true, true, false) => glyphs.commit_main_obsolete,
            (true, true, true) => glyphs.commit_main_obsolete_head,
        };

        let first_line = {
            let mut first_line = StyledString::new();
            first_line.append_plain(cursor);
            first_line.append_plain(" ");
            first_line.append(text);
            if is_head {
                set_effect(first_line, Effect::Bold)
            } else {
                first_line
            }
        };

        let mut lines = vec![first_line];
        let children: Vec<_> = current_node
            .children
            .iter()
            .filter(|child_oid| graph.nodes.contains_key(child_oid))
            .copied()
            .collect();
        for (child_idx, child_oid) in children.iter().enumerate() {
            if root_oids.contains(child_oid) {
                // Will be rendered by the parent.
                continue;
            }

            if child_idx == children.len() - 1 {
                let line = match last_child_line_char {
                    Some(_) => StyledString::plain(format!(
                        "{}{}",
                        glyphs.line_with_offshoot, glyphs.slash
                    )),

                    None => StyledString::plain(glyphs.line.to_string()),
                };
                lines.push(line)
            } else {
                lines.push(StyledString::plain(format!(
                    "{}{}",
                    glyphs.line_with_offshoot, glyphs.slash
                )))
            }

            let child_output = get_child_output(
                glyphs,
                graph,
                root_oids,
                commit_descriptors,
                head_oid,
                *child_oid,
                None,
            )?;
            for child_line in child_output {
                let line = if child_idx == children.len() - 1 {
                    match last_child_line_char {
                        Some(last_child_line_char) => StyledStringBuilder::new()
                            .append_plain(format!("{} ", last_child_line_char))
                            .append(child_line)
                            .build(),
                        None => child_line,
                    }
                } else {
                    StyledStringBuilder::new()
                        .append_plain(format!("{} ", glyphs.line))
                        .append(child_line)
                        .build()
                };
                lines.push(line)
            }
        }
        Ok(lines)
    }

    /// Render a pretty graph starting from the given root OIDs in the given graph.
    #[instrument(skip(commit_descriptors, graph))]
    fn get_output(
        glyphs: &Glyphs,
        dag: &Dag,
        graph: &SmartlogGraph,
        commit_descriptors: &mut [&mut dyn NodeDescriptor],
        head_oid: Option<NonZeroOid>,
        root_oids: &[NonZeroOid],
    ) -> eyre::Result<Vec<StyledString>> {
        let mut lines = Vec::new();

        // Determine if the provided OID has the provided parent OID as a parent.
        //
        // This returns `true` in strictly more cases than checking `graph`,
        // since there may be links between adjacent main branch commits which
        // are not reflected in `graph`.
        let has_real_parent = |oid: NonZeroOid, parent_oid: NonZeroOid| -> eyre::Result<bool> {
            let parents = dag.query().parents(CommitSet::from(oid))?;
            let result = parents.contains(&CommitVertex::from(parent_oid))?;
            Ok(result)
        };

        for (root_idx, root_oid) in root_oids.iter().enumerate() {
            if !dag
                .query()
                .parents(CommitSet::from(*root_oid))?
                .is_empty()?
            {
                let line = if root_idx > 0 && has_real_parent(*root_oid, root_oids[root_idx - 1])? {
                    StyledString::plain(glyphs.line.to_owned())
                } else {
                    StyledString::plain(glyphs.vertical_ellipsis.to_owned())
                };
                lines.push(line);
            } else if root_idx > 0 {
                // Pathological case: multiple topologically-unrelated roots.
                // Separate them with a newline.
                lines.push(StyledString::new());
            }

            let last_child_line_char = {
                if root_idx == root_oids.len() - 1 {
                    None
                } else {
                    let next_root_oid = root_oids[root_idx + 1];
                    if has_real_parent(next_root_oid, *root_oid)? {
                        Some(glyphs.line)
                    } else {
                        Some(glyphs.vertical_ellipsis)
                    }
                }
            };

            let child_output = get_child_output(
                glyphs,
                graph,
                root_oids,
                commit_descriptors,
                head_oid,
                *root_oid,
                last_child_line_char,
            )?;
            lines.extend(child_output.into_iter());
        }

        Ok(lines)
    }

    /// Render the smartlog graph and write it to the provided stream.
    #[instrument(skip(commit_descriptors, graph))]
    pub fn render_graph(
        effects: &Effects,
        repo: &Repo,
        dag: &Dag,
        graph: &SmartlogGraph,
        head_oid: Option<NonZeroOid>,
        commit_descriptors: &mut [&mut dyn NodeDescriptor],
    ) -> eyre::Result<Vec<StyledString>> {
        let root_oids = split_commit_graph_by_roots(effects, repo, dag, graph);
        let lines = get_output(
            effects.get_glyphs(),
            dag,
            graph,
            commit_descriptors,
            head_oid,
            &root_oids,
        )?;
        Ok(lines)
    }

    /// Options for rendering the smartlog.
    #[derive(Debug)]
    pub struct SmartlogOptions {
        /// Whether to also show commits in the smartlog which would normally not be
        /// visible.
        pub show_hidden_commits: bool,

        /// The point in time at which to show the smartlog. If not provided,
        /// renders the smartlog as of the current time. If negative, is treated
        /// as an offset from the current event.
        pub event_id: Option<isize>,

        /// The commits to render. These commits and their ancestors up to the
        /// main branch will be rendered.
        pub revset: Revset,
    }

    impl Default for SmartlogOptions {
        fn default() -> Self {
            Self {
                show_hidden_commits: Default::default(),
                event_id: Default::default(),
                revset: Revset("draft()".to_string()),
            }
        }
    }
}

/// Display a nice graph of commits you've recently worked on.
#[instrument]
pub fn smartlog(
    effects: &Effects,
    git_run_info: &GitRunInfo,
    options: &SmartlogOptions,
) -> eyre::Result<ExitCode> {
    let SmartlogOptions {
        show_hidden_commits,
        event_id,
        revset,
    } = options;

    let repo = Repo::from_dir(&git_run_info.working_directory)?;
    let head_info = repo.get_head_info()?;
    let conn = repo.get_db_conn()?;
    let event_log_db = EventLogDb::new(&conn)?;
    let event_replayer = EventReplayer::from_event_log_db(effects, &repo, &event_log_db)?;
    let (references_snapshot, event_cursor) = {
        let default_cursor = event_replayer.make_default_cursor();
        match event_id {
            None => (repo.get_references_snapshot()?, default_cursor),
            Some(event_id) => {
                let event_cursor = match event_id.cmp(&0) {
                    Ordering::Less => event_replayer.advance_cursor(default_cursor, *event_id),
                    Ordering::Equal | Ordering::Greater => event_replayer.make_cursor(*event_id),
                };
                let references_snapshot =
                    event_replayer.get_references_snapshot(&repo, event_cursor)?;
                (references_snapshot, event_cursor)
            }
        }
    };
    let mut dag = Dag::open_and_sync(
        effects,
        &repo,
        &event_replayer,
        event_cursor,
        &references_snapshot,
    )?;

    let observed_commits = {
        // For the purpose of resolving the revset expression, we may
        // temporarily clear the DAG's obsolete commit set. However, when we
        // render the smartlog later, we want to have the original obsolete
        // commit set so that we can correctly identify which of the nodes are
        // obsolete commits.
        let mut old_obsolete_commits = CommitSet::empty();
        if *show_hidden_commits {
            swap(&mut dag.obsolete_commits, &mut old_obsolete_commits);
        }
        let observed_commits = match resolve_commits(effects, &repo, &mut dag, vec![revset.clone()])
        {
            Ok(result) => match result.as_slice() {
                [commit_set] => commit_set.clone(),
                other => panic!(
                    "Expected exactly 1 result from resolve commits, got: {:?}",
                    other
                ),
            },
            Err(err) => {
                err.describe(effects)?;
                return Ok(ExitCode(1));
            }
        };
        if *show_hidden_commits {
            swap(&mut dag.obsolete_commits, &mut old_obsolete_commits);
        }
        observed_commits
    };

    let graph = make_smartlog_graph(
        effects,
        &repo,
        &dag,
        &event_replayer,
        event_cursor,
        &observed_commits,
        !show_hidden_commits,
    )?;

    let lines = render_graph(
        effects,
        &repo,
        &dag,
        &graph,
        references_snapshot.head_oid,
        &mut [
            &mut CommitOidDescriptor::new(true)?,
            &mut RelativeTimeDescriptor::new(&repo, SystemTime::now())?,
            &mut ObsolescenceExplanationDescriptor::new(
                &event_replayer,
                event_replayer.make_default_cursor(),
            )?,
            &mut BranchesDescriptor::new(
                &repo,
                &head_info,
                &references_snapshot,
                &Redactor::Disabled,
            )?,
            &mut DifferentialRevisionDescriptor::new(&repo, &Redactor::Disabled)?,
            &mut CommitMessageDescriptor::new(&Redactor::Disabled)?,
        ],
    )?;
    for line in lines {
        writeln!(
            effects.get_output_stream(),
            "{}",
            printable_styled_string(effects.get_glyphs(), line)?
        )?;
    }

    if !show_hidden_commits && get_hint_enabled(&repo, Hint::SmartlogFixAbandoned)? {
        let commits_with_abandoned_children: CommitSet = graph
            .nodes
            .iter()
            .filter_map(|(oid, node)| {
                if node.is_obsolete
                    && find_rewrite_target(&event_replayer, event_cursor, *oid).is_some()
                {
                    Some(*oid)
                } else {
                    None
                }
            })
            .collect();
        let children = dag.query().children(commits_with_abandoned_children)?;
        let num_abandoned_children = children.difference(&dag.obsolete_commits).count()?;
        if num_abandoned_children > 0 {
            writeln!(
                effects.get_output_stream(),
                "{}: there {} in your commit graph",
                style("hint").blue().bold(),
                Pluralize {
                    determiner: Some(("is", "are")),
                    amount: num_abandoned_children,
                    unit: ("abandoned commit", "abandoned commits"),
                },
            )?;
            writeln!(
                effects.get_output_stream(),
                "{}: to fix this, run: git restack",
                style("hint").blue().bold(),
            )?;
            print_hint_suppression_notice(effects, Hint::SmartlogFixAbandoned)?;
        }
    }

    Ok(ExitCode(0))
}
