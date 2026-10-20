//! Stacked pull requests (docs/stacks.md): native GitHub stacks and implicit base → head chains
//! among the loaded PRs, plus the plans the stack actions run. Pure, no `App` access.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::github::{MergeJob, PrDetail, PrSummary, StackMemberPr};

/// Seconds between merge-async polls, and how long to keep polling.
pub const MERGE_POLL: Duration = Duration::from_secs(2);
pub const MERGE_CAP: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A GitHub stack (`PullRequestStack`).
    Native { id: String, number: u64 },
    /// Found by matching each PR's base branch to another loaded PR's head branch.
    Implicit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub number: u64,
    /// 1-based, 1 = bottom (next to the trunk).
    pub position: usize,
    pub state: String,
    pub is_draft: bool,
    pub head: String,
    pub base: String,
    pub head_oid: String,
    pub title: String,
    /// In the loaded list (native stacks can reach beyond it).
    pub loaded: bool,
    pub ci: Option<String>,
    pub review: Option<String>,
    pub mergeable: Option<String>,
    pub merge_state: Option<String>,
}

impl Member {
    fn of_summary(p: &PrSummary, position: usize) -> Self {
        Self {
            id: p.id.clone(),
            number: p.number,
            position,
            state: p.state.clone(),
            is_draft: p.is_draft,
            head: p.head_ref_name.clone(),
            base: p.base_ref_name.clone(),
            head_oid: p.head_ref_oid.clone(),
            title: p.title.clone(),
            loaded: true,
            ci: p.ci_state().map(String::from),
            review: p.review_decision.clone(),
            mergeable: p.mergeable.clone(),
            merge_state: None,
        }
    }

    fn of_entry(e: &StackMemberPr, position: usize, loaded: bool) -> Self {
        Self {
            id: e.id.clone(),
            number: e.number,
            position,
            state: e.state.clone(),
            is_draft: e.is_draft,
            head: e.head_ref_name.clone(),
            base: e.base_ref_name.clone(),
            head_oid: e.head_ref_oid.clone(),
            title: e.title.clone(),
            loaded,
            ci: e.ci_state().map(String::from),
            review: e.review_decision.clone(),
            mergeable: e.mergeable.clone(),
            merge_state: e.merge_state_status.clone(),
        }
    }

    pub fn open(&self) -> bool {
        self.state == "OPEN"
    }

    pub fn merged(&self) -> bool {
        self.state == "MERGED"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// `owner/name`.
    pub repo: String,
    pub origin: Origin,
    /// The branch the bottom PR targets.
    pub trunk: String,
    /// GitHub's size for a native stack (members may be fewer when built from the list alone).
    pub size: usize,
    /// Bottom first.
    pub members: Vec<Member>,
}

impl Stack {
    pub fn native(&self) -> bool {
        matches!(self.origin, Origin::Native { .. })
    }

    /// The GitHub stack number.
    pub fn number(&self) -> Option<u64> {
        match &self.origin {
            Origin::Native { number, .. } => Some(*number),
            Origin::Implicit => None,
        }
    }

    /// Identity across rebuilds (caches, the needs-rebase links).
    pub fn key(&self) -> String {
        match &self.origin {
            Origin::Native { id, .. } => id.clone(),
            Origin::Implicit => format!("{}:{}", self.repo, self.members.iter().map(|m| m.number.to_string()).collect::<Vec<_>>().join(">")),
        }
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.members.iter().position(|m| m.id == id)
    }

    pub fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.id == id)
    }

    /// The member under `id`, or the trunk (`None`) for the bottom.
    pub fn parent_of(&self, id: &str) -> Option<&Member> {
        let i = self.index_of(id)?;
        i.checked_sub(1).map(|j| &self.members[j])
    }

    /// `(child head, its base)` for every open member: the needs-rebase compare query.
    pub fn link_pairs(&self) -> Vec<(String, String)> {
        self.members.iter().filter(|m| m.open() && !m.head.is_empty() && !m.base.is_empty()).map(|m| (m.head.clone(), m.base.clone())).collect()
    }
}

/// One `Ref.compare` result: `ahead` = commits on `parent` that `child` lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub child: String,
    pub parent: String,
    pub ahead: usize,
    pub behind: usize,
    pub status: String,
}

/// Commits `head` is missing from its base, when it needs a rebase.
pub fn needs_rebase(links: &[Link], head: &str) -> Option<usize> {
    links.iter().find(|l| l.child == head).map(|l| l.ahead).filter(|&n| n > 0)
}

/// Every stack among the loaded PRs: native ones (completed by a loaded detail's entries),
/// then implicit chains among the PRs no native stack claims. `is_trunk(repo, branch)` says
/// whether a branch is the repo's default one.
pub fn build(prs: &[PrSummary], details: &[&PrDetail], is_trunk: &dyn Fn(&str, &str) -> bool) -> Vec<Stack> {
    let loaded: HashSet<&str> = prs.iter().map(|p| p.id.as_str()).collect();
    // Native, from the list's fields, in first-appearance order.
    let mut natives: Vec<Stack> = Vec::new();
    for p in prs {
        let (Some(s), Some(e)) = (&p.stack, &p.stack_entry) else { continue };
        let i = match natives.iter().position(|n| matches!(&n.origin, Origin::Native { id, .. } if *id == s.id)) {
            Some(i) => i,
            None => {
                natives.push(Stack {
                    repo: p.repository.name_with_owner.clone(),
                    origin: Origin::Native { id: s.id.clone(), number: s.number },
                    trunk: s.base_ref_name.clone(),
                    size: s.size,
                    members: Vec::new(),
                });
                natives.len() - 1
            }
        };
        if natives[i].members.iter().all(|m| m.id != p.id) {
            natives[i].members.push(Member::of_summary(p, e.position));
        }
    }
    // A loaded detail knows every entry; it replaces the list's view of its stack.
    for d in details {
        let Some(full) = from_detail(d, &loaded) else { continue };
        if !full.members.iter().any(|m| m.loaded) {
            continue;
        }
        match natives.iter().position(|n| n.origin == full.origin) {
            Some(i) => natives[i] = full,
            None => natives.push(full),
        }
    }
    for s in &mut natives {
        s.members.sort_by_key(|m| m.position);
        s.size = s.size.max(s.members.len());
    }
    let claimed: HashSet<&str> = natives.iter().flat_map(|s| s.members.iter().map(|m| m.id.as_str())).collect();
    let free: Vec<&PrSummary> = prs.iter().filter(|p| !claimed.contains(p.id.as_str())).collect();
    let mut out = natives;
    out.extend(chains(&free, is_trunk));
    out
}

/// A detail's native stack, every entry included (`loaded` says which are in the list).
pub fn from_detail(d: &PrDetail, loaded: &HashSet<&str>) -> Option<Stack> {
    let s = d.stack.as_ref()?;
    let mut members: Vec<Member> =
        s.entries.iter().filter_map(|e| e.pull_request.as_ref().map(|p| Member::of_entry(p, e.position, loaded.contains(p.id.as_str())))).collect();
    members.sort_by_key(|m| m.position);
    Some(Stack {
        repo: d.repository.name_with_owner.clone(),
        origin: Origin::Native { id: s.id.clone(), number: s.number },
        trunk: s.base_ref_name.clone(),
        size: s.size.max(members.len()),
        members,
    })
}

/// Implicit chains: `parent(p)` is the same-repo PR whose head branch is `p`'s base (OPEN over
/// MERGED over CLOSED, newest on ties); a PR on the trunk, or whose parent isn't loaded, is a
/// root; each root plus its descendants is a chain, and a root without children is no stack.
/// A branch with two children continues with the preferred one; the other starts its own chain.
pub fn chains(prs: &[&PrSummary], is_trunk: &dyn Fn(&str, &str) -> bool) -> Vec<Stack> {
    let n = prs.len();
    let rank = |p: &PrSummary| match p.state.as_str() {
        "OPEN" => 0u8,
        "MERGED" => 1,
        _ => 2,
    };
    let better = |a: usize, b: usize| (rank(prs[a]), std::cmp::Reverse(prs[a].number)) < (rank(prs[b]), std::cmp::Reverse(prs[b].number));
    let repo = |p: &PrSummary| p.repository.name_with_owner.to_lowercase();
    // Fork PRs never chain: their branch isn't in the base repo, so no GitHub stack can hold them.
    let mut heads: HashMap<(String, &str), Vec<usize>> = HashMap::new();
    for (i, p) in prs.iter().enumerate() {
        if !p.is_cross_repository && !p.head_ref_name.is_empty() {
            heads.entry((repo(p), p.head_ref_name.as_str())).or_default().push(i);
        }
    }
    let mut parent: Vec<Option<usize>> = prs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if p.is_cross_repository || p.base_ref_name.is_empty() || is_trunk(&p.repository.name_with_owner, &p.base_ref_name) {
                return None;
            }
            let cands = heads.get(&(repo(p), p.base_ref_name.as_str()))?;
            cands.iter().copied().filter(|&c| c != i).reduce(|a, b| if better(b, a) { b } else { a })
        })
        .collect();
    // Cut cycles at the first repeat.
    let mut state = vec![0u8; n];
    for start in 0..n {
        let mut path = Vec::new();
        let mut cur = Some(start);
        while let Some(c) = cur {
            match state[c] {
                2 => break,
                1 => {
                    if let Some(&last) = path.last() {
                        parent[last] = None;
                    }
                    break;
                }
                _ => {}
            }
            state[c] = 1;
            path.push(c);
            cur = parent[c];
        }
        for c in path {
            state[c] = 2;
        }
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, p) in parent.iter().enumerate() {
        if let Some(p) = p {
            children[*p].push(i);
        }
    }
    for c in &mut children {
        c.sort_by(|&a, &b| (rank(prs[a]), std::cmp::Reverse(prs[a].number)).cmp(&(rank(prs[b]), std::cmp::Reverse(prs[b].number))));
    }
    let mut roots: Vec<usize> = (0..n).filter(|&i| parent[i].is_none()).collect();
    // Second and later children start chains of their own.
    for c in &children {
        roots.extend(c.iter().skip(1));
    }
    roots.sort_unstable();
    let mut out = Vec::new();
    for root in roots {
        let mut chain = vec![root];
        let mut cur = root;
        while let Some(&next) = children[cur].first() {
            if chain.contains(&next) {
                break;
            }
            chain.push(next);
            cur = next;
        }
        if chain.len() < 2 {
            continue;
        }
        let members: Vec<Member> = chain.iter().enumerate().map(|(k, &i)| Member::of_summary(prs[i], k + 1)).collect();
        out.push(Stack {
            repo: prs[root].repository.name_with_owner.clone(),
            origin: Origin::Implicit,
            trunk: prs[root].base_ref_name.clone(),
            size: members.len(),
            members,
        });
    }
    out
}

/// PR id → (stack index, member index), for every member of every stack.
pub fn membership(stacks: &[Stack]) -> HashMap<String, (usize, usize)> {
    let mut out = HashMap::new();
    for (s, st) in stacks.iter().enumerate() {
        for (m, mem) in st.members.iter().enumerate() {
            out.entry(mem.id.clone()).or_insert((s, m));
        }
    }
    out
}

/// A list row once stacks are grouped: the PR (index into `prs`) and its tree glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowView {
    pub idx: usize,
    /// `├` for a middle member, `└` for the top one; none for the bottom or an unstacked PR.
    pub glyph: Option<&'static str>,
}

/// `order` (indices into `prs`, already filtered and sorted) with each stack's shown members
/// together, bottom first, at the rank of its first member in `order`.
pub fn grouped(order: &[usize], prs: &[PrSummary], stacks: &[Stack], m: &HashMap<String, (usize, usize)>) -> Vec<RowView> {
    let shown: HashMap<&str, usize> = order.iter().map(|&i| (prs[i].id.as_str(), i)).collect();
    let mut done: HashSet<usize> = HashSet::new();
    let mut out = Vec::with_capacity(order.len());
    for &i in order {
        let Some(&(s, _)) = m.get(&prs[i].id) else {
            out.push(RowView { idx: i, glyph: None });
            continue;
        };
        if !done.insert(s) {
            continue;
        }
        let rows: Vec<usize> = stacks[s].members.iter().filter_map(|mem| shown.get(mem.id.as_str()).copied()).collect();
        let n = rows.len();
        for (k, idx) in rows.into_iter().enumerate() {
            let glyph = match k {
                0 => None,
                _ if k + 1 == n => Some("└"),
                _ => Some("├"),
            };
            out.push(RowView { idx, glyph });
        }
    }
    out
}

/// The member below (toward the trunk, `up == false`) or above `id`; `None` at the ends.
pub fn neighbour<'a>(s: &'a Stack, id: &str, up: bool) -> Option<&'a Member> {
    let i = s.index_of(id)?;
    if up { s.members.get(i + 1) } else { i.checked_sub(1).map(|j| &s.members[j]) }
}

/// The PR numbers to link an implicit chain into a GitHub stack, bottom first: merged bottom
/// members are skipped, the rest must be open (drafts are fine) and each one's base must be the
/// previous one's head.
pub fn link_plan(s: &Stack) -> Result<Vec<u64>, String> {
    if let Some(n) = s.number() {
        return Err(format!("already linked (stack {n})"));
    }
    let rest: Vec<&Member> = s.members.iter().skip_while(|m| m.merged()).collect();
    if let Some(m) = rest.iter().find(|m| !m.open()) {
        return Err(format!("#{} is {}", m.number, m.state.to_lowercase()));
    }
    if rest.len() < 2 {
        return Err("needs at least two open PRs".into());
    }
    for w in rest.windows(2) {
        if w[1].base != w[0].head {
            return Err(format!("#{} targets {}, not #{}'s branch {}", w[1].number, w[1].base, w[0].number, w[0].head));
        }
    }
    Ok(rest.iter().map(|m| m.number).collect())
}

/// The open members merging "up to" `id` takes with it (bottom first). Native stacks only:
/// merging an implicit chain's bottom would leave the next PR targeting a deleted branch.
pub fn merge_plan<'a>(s: &'a Stack, id: &str) -> Result<Vec<&'a Member>, String> {
    if !s.native() {
        return Err("not a GitHub stack: link it first (L)".into());
    }
    let i = s.index_of(id).ok_or("not in this stack")?;
    let plan: Vec<&Member> = s.members[..=i].iter().filter(|m| !m.merged()).collect();
    if let Some(m) = plan.iter().find(|m| m.state == "CLOSED") {
        return Err(format!("#{} is closed", m.number));
    }
    if let Some(m) = plan.iter().find(|m| m.is_draft) {
        return Err(format!("#{} is a draft", m.number));
    }
    if plan.is_empty() {
        return Err("nothing left to merge".into());
    }
    Ok(plan)
}

/// What to do after a merge-async reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStep {
    Merged,
    Enqueued,
    Failed(String),
    /// Still pending: ask again for this uuid.
    Poll(String),
    TimedOut,
}

pub fn merge_step(job: &MergeJob, waited: Duration) -> MergeStep {
    match job.status.as_str() {
        "merged" => MergeStep::Merged,
        "enqueued" => MergeStep::Enqueued,
        "failed" => MergeStep::Failed(job.message.clone().unwrap_or_else(|| "the merge failed".into())),
        _ if waited >= MERGE_CAP => MergeStep::TimedOut,
        _ => match &job.uuid {
            Some(u) => MergeStep::Poll(u.clone()),
            None => MergeStep::Failed("GitHub returned no merge request id to follow".into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{StackEntryRef, StackRef};
    use crate::search::tests::pr;

    fn chain_pr(n: u64, repo: &str, head: &str, base: &str, state: &str) -> PrSummary {
        let mut p = pr(n, repo, &format!("pr {n}"), "2026-10-19T00:00:00Z");
        p.head_ref_name = head.into();
        p.base_ref_name = base.into();
        p.state = state.into();
        p
    }

    fn main_trunk(_: &str, b: &str) -> bool {
        b == "main"
    }

    fn numbers(s: &Stack) -> Vec<u64> {
        s.members.iter().map(|m| m.number).collect()
    }

    fn ours() -> Vec<PrSummary> {
        vec![
            chain_pr(4, "a/r", "d", "c", "OPEN"),
            chain_pr(3, "a/r", "c", "b", "OPEN"),
            chain_pr(2, "a/r", "b", "a", "OPEN"),
            chain_pr(1, "a/r", "a", "main", "OPEN"),
            chain_pr(9, "a/r", "solo", "main", "OPEN"),
        ]
    }

    #[test]
    fn chains_follow_base_to_head() {
        let prs = ours();
        let s = build(&prs, &[], &main_trunk);
        assert_eq!(s.len(), 1);
        assert_eq!(numbers(&s[0]), vec![1, 2, 3, 4]);
        assert_eq!((s[0].trunk.as_str(), s[0].origin.clone(), s[0].size), ("main", Origin::Implicit, 4));
        assert_eq!(s[0].members.iter().map(|m| m.position).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn open_head_beats_an_older_closed_pr_on_the_same_branch() {
        let mut prs = ours();
        prs.push(chain_pr(7, "a/r", "b", "a", "CLOSED"));
        prs.push(chain_pr(0, "a/r", "b", "a", "MERGED"));
        let s = chains(&prs.iter().collect::<Vec<_>>(), &main_trunk);
        assert_eq!(numbers(&s[0]), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_merged_parent_stays_until_the_child_is_retargeted() {
        let mut prs = ours();
        prs[3].state = "MERGED".into();
        let s = chains(&prs.iter().collect::<Vec<_>>(), &main_trunk);
        assert_eq!(numbers(&s[0]), vec![1, 2, 3, 4]);
        assert!(s[0].members[0].merged());
        // Retargeted to main: #2 is a root now, #1 stands alone.
        prs[2].base_ref_name = "main".into();
        let s = chains(&prs.iter().collect::<Vec<_>>(), &main_trunk);
        assert_eq!(s.len(), 1);
        assert_eq!(numbers(&s[0]), vec![2, 3, 4]);
    }

    #[test]
    fn forks_and_other_repos_never_chain() {
        let mut prs = vec![chain_pr(1, "a/r", "a", "main", "OPEN"), chain_pr(2, "b/r", "b", "a", "OPEN"), chain_pr(3, "a/r", "x", "fork-branch", "OPEN")];
        let mut fork = chain_pr(4, "a/r", "fork-branch", "main", "OPEN");
        fork.is_cross_repository = true;
        prs.push(fork);
        // Nor does a fork PR that targets one of ours.
        let mut child = chain_pr(5, "a/r", "a", "a", "OPEN");
        child.is_cross_repository = true;
        prs.push(child);
        assert!(chains(&prs.iter().collect::<Vec<_>>(), &main_trunk).is_empty());
    }

    #[test]
    fn cycles_end_at_the_repeat_and_singles_are_no_stack() {
        let prs = [chain_pr(1, "a/r", "a", "b", "OPEN"), chain_pr(2, "a/r", "b", "a", "OPEN"), chain_pr(3, "a/r", "z", "main", "OPEN")];
        let s = chains(&prs.iter().collect::<Vec<_>>(), &main_trunk);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].members.len(), 2);
    }

    #[test]
    fn a_fork_in_the_chain_starts_a_second_chain() {
        let mut prs = ours();
        prs.push(chain_pr(5, "a/r", "e", "b", "OPEN"));
        let s = chains(&prs.iter().collect::<Vec<_>>(), &main_trunk);
        // #5 is newer than #3, so it continues #1 → #2; #3 → #4 stands on its own (listed first, as
        // #3 comes before #1 in the list).
        assert_eq!(s.iter().map(numbers).collect::<Vec<_>>(), vec![vec![3, 4], vec![1, 2, 5]]);
    }

    fn native(n: u64, pos: usize) -> PrSummary {
        let mut p = chain_pr(n, "o/x", &format!("h{n}"), "main", "OPEN");
        p.stack = Some(StackRef { id: "PRS_1".into(), number: 196, size: 3, base_ref_name: "main".into() });
        p.stack_entry = Some(StackEntryRef { position: pos });
        p
    }

    fn detail_with_stack() -> PrDetail {
        let entry = |n: u64, pos: usize, state: &str| {
            serde_json::json!({ "position": pos, "pullRequest": { "id": format!("o/x#{n}"), "number": n, "title": format!("pr {n}"), "state": state,
                "isDraft": false, "headRefName": format!("h{n}"), "baseRefName": if pos == 1 { "main".to_string() } else { format!("h{}", n - 1) },
                "headRefOid": "abc", "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "reviewDecision": null,
                "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "SUCCESS" } } }] } } })
        };
        serde_json::from_value(serde_json::json!({
            "id": "o/x#2", "number": 2, "title": "t", "body": "", "bodyHTML": "", "state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
            "createdAt": "2026-10-18T00:00:00Z", "updatedAt": "2026-10-19T01:00:00Z", "author": null, "headRefName": "h2", "baseRefName": "h1",
            "additions": 1, "deletions": 1, "changedFiles": 1, "headRefOid": "abc", "canBeRebased": true, "viewerCanClose": true,
            "viewerCanReopen": false, "viewerCanMergeAsAdmin": false, "labels": { "nodes": [] }, "assignees": { "nodes": [] },
            "reviewRequests": { "nodes": [] }, "participants": { "nodes": [] },
            "repository": { "nameWithOwner": "o/x", "mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": true,
                "mentionableUsers": { "nodes": [] }, "defaultBranchRef": { "name": "main" } },
            "commits": { "totalCount": 1, "nodes": [] }, "reviews": { "nodes": [] }, "comments": { "nodes": [] }, "reviewThreads": { "nodes": [] },
            "stackEntry": { "position": 2 },
            "stack": { "id": "PRS_1", "number": 196, "size": 3, "baseRefName": "main",
                "entries": { "totalCount": 3, "nodes": [entry(3, 3, "OPEN"), entry(1, 1, "MERGED"), entry(2, 2, "OPEN")] } }
        }))
        .unwrap()
    }

    #[test]
    fn natives_from_the_list_then_the_detail() {
        // Only #2 and #3 are loaded; the list knows their positions, the detail every entry.
        let prs = vec![native(3, 3), native(2, 2), chain_pr(9, "o/x", "q", "h3", "OPEN")];
        let s = build(&prs, &[], &main_trunk);
        assert_eq!(numbers(&s[0]), vec![2, 3]);
        assert_eq!((s[0].size, s[0].number()), (3, Some(196)));
        // #9 is based on the stack's top, but native members never join a chain.
        assert_eq!(s.len(), 1);
        let d = detail_with_stack();
        let s = build(&prs, &[&d], &main_trunk);
        assert_eq!(numbers(&s[0]), vec![1, 2, 3]);
        assert_eq!(s[0].members.iter().map(|m| m.loaded).collect::<Vec<_>>(), vec![false, true, true]);
        assert_eq!(s[0].members[0].ci.as_deref(), Some("SUCCESS"));
        let m = membership(&s);
        assert_eq!(m["o/x#1"], (0, 0));
        // A detail whose stack has no loaded member (another repo filter) adds nothing.
        assert!(build(&prs[2..], &[&d], &main_trunk).is_empty());
    }

    #[test]
    fn grouping_keeps_stacks_together_bottom_first() {
        let prs = ours();
        // Newest first: #4, #3, #2, #1, #9; an unrelated PR in between.
        let mut list = prs.clone();
        list.insert(1, chain_pr(8, "a/r", "y", "main", "OPEN"));
        let stacks = build(&list, &[], &main_trunk);
        let m = membership(&stacks);
        let order: Vec<usize> = (0..list.len()).collect();
        let rows = grouped(&order, &list, &stacks, &m);
        let shown: Vec<(u64, Option<&str>)> = rows.iter().map(|r| (list[r.idx].number, r.glyph)).collect();
        assert_eq!(shown, vec![(1, None), (2, Some("├")), (3, Some("├")), (4, Some("└")), (8, None), (9, None)]);
        // A filter that hides #2 and #3 (the unread filter, a pinned search) composes.
        let order: Vec<usize> = vec![5, 0, 4];
        let rows = grouped(&order, &list, &stacks, &m);
        let shown: Vec<(u64, Option<&str>)> = rows.iter().map(|r| (list[r.idx].number, r.glyph)).collect();
        assert_eq!(shown, vec![(9, None), (1, None), (4, Some("└"))]);
        // One member alone has no glyph.
        let rows = grouped(&[2], &list, &stacks, &m);
        assert_eq!(rows, vec![RowView { idx: 2, glyph: None }]);
    }

    #[test]
    fn neighbours_stop_at_the_ends() {
        let s = &build(&ours(), &[], &main_trunk)[0];
        let id = |n: u64| s.members.iter().find(|m| m.number == n).unwrap().id.clone();
        assert_eq!(neighbour(s, &id(2), true).map(|m| m.number), Some(3));
        assert_eq!(neighbour(s, &id(2), false).map(|m| m.number), Some(1));
        assert!(neighbour(s, &id(1), false).is_none());
        assert!(neighbour(s, &id(4), true).is_none());
        assert_eq!(s.parent_of(&id(1)), None);
        assert_eq!(s.parent_of(&id(3)).map(|m| m.number), Some(2));
    }

    #[test]
    fn link_plans() {
        let mut prs = ours();
        let s = &build(&prs, &[], &main_trunk)[0];
        assert_eq!(link_plan(s), Ok(vec![1, 2, 3, 4]));
        // A draft is fine; a merged bottom is skipped.
        prs[1].is_draft = true;
        prs[3].state = "MERGED".into();
        let s = &chains(&prs.iter().collect::<Vec<_>>(), &main_trunk)[0];
        assert_eq!(link_plan(s), Ok(vec![2, 3, 4]));
        // A closed member blocks; so does a gap between bases and heads.
        let mut gap = s.clone();
        gap.members[2].base = "elsewhere".into();
        assert!(link_plan(&gap).unwrap_err().contains("#3 targets elsewhere"));
        let mut closed = s.clone();
        closed.members[3].state = "CLOSED".into();
        assert_eq!(link_plan(&closed), Err("#4 is closed".into()));
        let natives = build(&[native(2, 2), native(3, 3)], &[], &main_trunk);
        assert_eq!(link_plan(&natives[0]), Err("already linked (stack 196)".into()));
    }

    #[test]
    fn merge_plans() {
        let d = detail_with_stack();
        let prs = vec![native(3, 3), native(2, 2)];
        let s = &build(&prs, &[&d], &main_trunk)[0];
        // #1 is merged: merging up to #3 takes #2 and #3.
        let plan: Vec<u64> = merge_plan(s, "o/x#3").unwrap().iter().map(|m| m.number).collect();
        assert_eq!(plan, vec![2, 3]);
        let mut draft = s.clone();
        draft.members[1].is_draft = true;
        assert_eq!(merge_plan(&draft, "o/x#3").map(|_| ()), Err("#2 is a draft".into()));
        let mut closed = s.clone();
        closed.members[2].state = "CLOSED".into();
        assert_eq!(merge_plan(&closed, "o/x#3").map(|_| ()), Err("#3 is closed".into()));
        assert_eq!(merge_plan(s, "o/x#1").map(|_| ()), Err("nothing left to merge".into()));
        let implicit = &build(&ours(), &[], &main_trunk)[0];
        assert!(merge_plan(implicit, &implicit.members[1].id).unwrap_err().contains("link it first"));
    }

    #[test]
    fn needs_rebase_from_ahead_by() {
        let links = vec![
            Link { child: "b".into(), parent: "a".into(), ahead: 2, behind: 1, status: "DIVERGED".into() },
            Link { child: "c".into(), parent: "b".into(), ahead: 0, behind: 1, status: "BEHIND".into() },
        ];
        assert_eq!(needs_rebase(&links, "b"), Some(2));
        assert_eq!(needs_rebase(&links, "c"), None);
        assert_eq!(needs_rebase(&links, "zzz"), None);
    }

    #[test]
    fn merge_async_state_machine() {
        let job = |status: &str, uuid: Option<&str>, msg: Option<&str>| MergeJob { status: status.into(), uuid: uuid.map(String::from), message: msg.map(String::from) };
        let t = Duration::from_secs(4);
        assert_eq!(merge_step(&job("pending", Some("u1"), None), t), MergeStep::Poll("u1".into()));
        assert_eq!(merge_step(&job("merged", None, None), t), MergeStep::Merged);
        assert_eq!(merge_step(&job("enqueued", Some("u1"), None), t), MergeStep::Enqueued);
        assert_eq!(merge_step(&job("failed", Some("u1"), Some("Head branch was modified")), t), MergeStep::Failed("Head branch was modified".into()));
        assert_eq!(merge_step(&job("pending", Some("u1"), None), MERGE_CAP), MergeStep::TimedOut);
        assert!(matches!(merge_step(&job("pending", None, None), t), MergeStep::Failed(_)));
    }

    #[test]
    fn link_pairs_cover_open_members() {
        let mut prs = ours();
        prs[3].state = "MERGED".into();
        let s = &chains(&prs.iter().collect::<Vec<_>>(), &main_trunk)[0];
        assert_eq!(s.link_pairs(), vec![("b".to_string(), "a".to_string()), ("c".into(), "b".into()), ("d".into(), "c".into())]);
    }
}
