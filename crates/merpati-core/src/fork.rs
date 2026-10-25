//! Local collection forks (PRD section 4, "duplikasi dan fork lokal").
//!
//! A fork is an ordinary collection with its own item ids (secrets are
//! stored per item id, so sharing ids would make the fork and its parent
//! overwrite each other's tokens). A [`ForkLink`] maps fork ids to parent ids
//! and keeps a snapshot of the parent from the last sync, which makes pulling
//! and merging three-way: a change on one side is applied, a change on both
//! sides is a conflict the user resolves.
//!
//! Comparisons ignore secret values (they are blanked in the snapshot, which
//! lives in the workspace's local state).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::model::{Collection, Folder, Id, Item, RequestItem};
use crate::util::new_id;
use crate::variables::Variable;

/// Pseudo item id for the collection's own settings (auth, scripts, variables).
pub const ROOT: &str = "";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ForkLink {
    pub fork: Id,
    pub parent: Id,
    pub forked_at_ms: i64,
    /// Fork item id → parent item id.
    #[serde(default)]
    pub ids: BTreeMap<Id, Id>,
    /// The parent at the last sync, in parent ids, secrets blanked.
    pub base: Collection,
}

/// Copies `parent` as a fork named `name`, with fresh ids.
pub fn create_fork(parent: &Collection, name: &str, now_ms: i64) -> (Collection, ForkLink) {
    let mut fork = parent.clone();
    fork.id = new_id();
    fork.name = name.to_string();
    let mut ids = BTreeMap::new();
    fn renew(items: &mut [Item], ids: &mut BTreeMap<Id, Id>) {
        for item in items {
            let old = item.id().to_string();
            let fresh = new_id();
            ids.insert(fresh.clone(), old);
            match item {
                Item::Folder(f) => {
                    f.id = fresh;
                    renew(&mut f.items, ids);
                }
                // Example ids stay: nothing is stored per example, and equal
                // ids keep unchanged examples equal in comparisons.
                Item::Request(r) => r.id = fresh,
            }
        }
    }
    renew(&mut fork.items, &mut ids);
    let link =
        ForkLink { fork: fork.id.clone(), parent: parent.id.clone(), forked_at_ms: now_ms, ids, base: scrub(parent) };
    (fork, link)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
    /// Changed on both sides (or changed on one side, removed on the other).
    Conflict,
}

/// One difference between the source and the target, in parent ids.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanEntry {
    /// Item id in parent ids; [`ROOT`] for collection settings.
    pub id: Id,
    pub name: String,
    pub is_folder: bool,
    pub kind: ChangeKind,
    /// Apply this entry. Defaults to true except for conflicts.
    pub apply: bool,
    /// For conflicts: take the source side when applied.
    pub take_source: bool,
    /// Pretty JSON of the target and source versions (secrets blanked), for diffs.
    pub target_text: String,
    pub source_text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn conflicts(&self) -> usize {
        self.entries.iter().filter(|e| e.kind == ChangeKind::Conflict).count()
    }
}

/// Parent changes since the last sync that are not in the fork yet.
pub fn plan_pull(link: &ForkLink, parent: &Collection, fork: &Collection) -> Plan {
    plan(&link.base, parent, &to_parent_ids(fork, link))
}

/// Fork changes since the last sync that are not in the parent yet.
pub fn plan_merge(link: &ForkLink, parent: &Collection, fork: &Collection) -> Plan {
    plan(&link.base, &to_parent_ids(fork, link), parent)
}

/// Applies the chosen entries of a [`plan_pull`] to the fork.
pub fn apply_pull(link: &mut ForkLink, plan: &Plan, parent: &Collection, fork: &mut Collection) {
    let mut target = to_parent_ids(fork, link);
    apply(plan, parent, &mut target);
    *fork = from_parent_ids(target, fork, link);
    link.base = scrub(parent);
}

/// Applies the chosen entries of a [`plan_merge`] to the parent.
pub fn apply_merge(link: &mut ForkLink, plan: &Plan, fork: &Collection, parent: &mut Collection) {
    let source = to_parent_ids(fork, link);
    apply(plan, &source, parent);
    // Items new to the parent still carry fork ids: give them their own.
    let local: HashSet<Id> = flatten(fork).into_keys().filter(|id| !link.ids.contains_key(id)).collect();
    fn rename(items: &mut [Item], local: &HashSet<Id>, ids: &mut BTreeMap<Id, Id>) {
        for item in items {
            if local.contains(item.id()) {
                let fresh = new_id();
                ids.insert(item.id().to_string(), fresh.clone());
                match item {
                    Item::Folder(f) => f.id = fresh,
                    Item::Request(r) => r.id = fresh,
                }
            }
            if let Item::Folder(f) = item {
                rename(&mut f.items, local, ids);
            }
        }
    }
    rename(&mut parent.items, &local, &mut link.ids);
    link.base = scrub(parent);
}

// ---------------------------------------------------------------------------
// Three-way comparison
// ---------------------------------------------------------------------------

// Short-lived (one per item while comparing), so boxing would only add work.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
enum Content {
    /// Folder settings without children.
    Folder(Folder),
    Request(RequestItem),
    Root(Collection),
}

#[derive(Clone, Debug, PartialEq)]
struct Node {
    parent: Option<Id>,
    depth: usize,
    content: Content,
}

impl Node {
    fn name(&self) -> String {
        match &self.content {
            Content::Folder(f) => f.name.clone(),
            Content::Request(r) => r.name.clone(),
            Content::Root(_) => "Collection settings".into(),
        }
    }

    /// Content and location with secrets blanked, for comparisons.
    fn key(&self) -> (Option<Id>, String) {
        let c = match &self.content {
            Content::Folder(f) => {
                let mut f = f.clone();
                scrub_auth(&mut f.auth);
                scrub_vars(&mut f.variables);
                serde_json::to_value(&f)
            }
            Content::Request(r) => {
                let mut r = r.clone();
                scrub_request(&mut r);
                serde_json::to_value(&r)
            }
            Content::Root(c) => serde_json::to_value(root_settings(&scrub(c))),
        };
        (self.parent.clone(), c.map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()).unwrap_or_default())
    }
}

/// Collection settings compared by the root entry (no name, no items).
fn root_settings(c: &Collection) -> Collection {
    Collection { id: String::new(), name: String::new(), items: Vec::new(), ..c.clone() }
}

fn flatten(c: &Collection) -> HashMap<Id, Node> {
    fn walk(items: &[Item], parent: Option<&str>, depth: usize, out: &mut HashMap<Id, Node>) {
        for item in items {
            let content = match item {
                Item::Folder(f) => Content::Folder(Folder { items: Vec::new(), ..f.clone() }),
                Item::Request(r) => Content::Request(r.clone()),
            };
            out.insert(item.id().to_string(), Node { parent: parent.map(str::to_string), depth, content });
            if let Item::Folder(f) = item {
                walk(&f.items, Some(&f.id), depth + 1, out);
            }
        }
    }
    let mut out = HashMap::new();
    out.insert(ROOT.to_string(), Node { parent: None, depth: 0, content: Content::Root(root_settings(c)) });
    walk(&c.items, None, 1, &mut out);
    out
}

fn plan(base: &Collection, source: &Collection, target: &Collection) -> Plan {
    let (b, s, t) = (flatten(base), flatten(source), flatten(target));
    let mut ids: Vec<&Id> = b.keys().chain(s.keys()).collect::<HashSet<_>>().into_iter().collect();
    // Parents before children, then by name for a stable list.
    ids.sort_by_key(|id| {
        let n = s.get(*id).or_else(|| b.get(*id));
        (n.map(|n| n.depth).unwrap_or(0), n.map(|n| n.name()).unwrap_or_default(), (*id).clone())
    });
    let mut entries = Vec::new();
    for id in ids {
        let (bn, sn, tn) = (b.get(id), s.get(id), t.get(id));
        let kind = match (bn, sn, tn) {
            (Some(bn), Some(sn), Some(tn)) => {
                let (bk, sk, tk) = (bn.key(), sn.key(), tn.key());
                if sk == bk || sk == tk {
                    continue;
                }
                if tk == bk { ChangeKind::Modified } else { ChangeKind::Conflict }
            }
            // Removed on the target: re-adding a changed item is a conflict.
            (Some(bn), Some(sn), None) => {
                if sn.key() == bn.key() {
                    continue;
                }
                ChangeKind::Conflict
            }
            (None, Some(sn), tn) => match tn {
                None => ChangeKind::Added,
                Some(tn) if tn.key() == sn.key() => continue,
                Some(_) => ChangeKind::Conflict,
            },
            (Some(bn), None, Some(tn)) => {
                if tn.key() == bn.key() {
                    ChangeKind::Removed
                } else {
                    ChangeKind::Conflict
                }
            }
            (Some(_), None, None) | (None, None, _) => continue,
        };
        let node = sn.or(bn).expect("one side has the item");
        entries.push(PlanEntry {
            id: id.clone(),
            name: node.name(),
            is_folder: matches!(node.content, Content::Folder(_)),
            kind,
            apply: kind != ChangeKind::Conflict,
            take_source: kind != ChangeKind::Conflict,
            target_text: tn.map(|n| n.key().1).unwrap_or_default(),
            source_text: sn.map(|n| n.key().1).unwrap_or_default(),
        });
    }
    Plan { entries }
}

fn apply(plan: &Plan, source: &Collection, target: &mut Collection) {
    let s = flatten(source);
    // The plan is sorted by name for display. Apply removals first, then
    // parents before children and siblings in source order, so that each
    // insert at its source index keeps the source's order.
    let mut entries: Vec<&PlanEntry> =
        plan.entries.iter().filter(|e| e.apply && (e.kind != ChangeKind::Conflict || e.take_source)).collect();
    entries.sort_by_cached_key(|e| match s.get(&e.id) {
        None => (0, 0, 0),
        Some(n) => (1, n.depth, position(source, &e.id)),
    });
    for e in entries {
        if e.id == ROOT {
            if let Some(Node { content: Content::Root(r), .. }) = s.get(ROOT) {
                target.description = r.description.clone();
                target.auth = r.auth.clone();
                target.scripts = r.scripts.clone();
                target.variables = r.variables.clone();
            }
            continue;
        }
        match s.get(&e.id) {
            // Removed in the source.
            None => {
                remove(&mut target.items, &e.id);
            }
            Some(node) => {
                let existing = take(&mut target.items, &e.id);
                let item = match (&node.content, existing) {
                    (Content::Request(r), _) => Item::Request(r.clone()),
                    (Content::Folder(f), Some(Item::Folder(old))) => {
                        Item::Folder(Folder { items: old.items, ..f.clone() })
                    }
                    (Content::Folder(f), _) => Item::Folder(f.clone()),
                    (Content::Root(_), _) => continue,
                };
                let index = position(source, &e.id);
                insert(target, node.parent.as_deref(), item, index);
            }
        }
    }
}

fn remove(items: &mut Vec<Item>, id: &str) -> bool {
    take(items, id).is_some()
}

fn take(items: &mut Vec<Item>, id: &str) -> Option<Item> {
    if let Some(i) = items.iter().position(|it| it.id() == id) {
        return Some(items.remove(i));
    }
    items.iter_mut().find_map(|it| match it {
        Item::Folder(f) => take(&mut f.items, id),
        Item::Request(_) => None,
    })
}

fn position(c: &Collection, id: &str) -> usize {
    fn find(items: &[Item], id: &str) -> Option<usize> {
        items.iter().position(|it| it.id() == id).or_else(|| {
            items.iter().find_map(|it| match it {
                Item::Folder(f) => find(&f.items, id),
                Item::Request(_) => None,
            })
        })
    }
    find(&c.items, id).unwrap_or(usize::MAX)
}

/// Inserts under `parent` (root when it does not exist in the target).
fn insert(target: &mut Collection, parent: Option<&str>, item: Item, index: usize) {
    fn folder<'a>(items: &'a mut [Item], id: &str) -> Option<&'a mut Folder> {
        for it in items {
            if let Item::Folder(f) = it {
                if f.id == id {
                    return Some(f);
                }
                if let Some(found) = folder(&mut f.items, id) {
                    return Some(found);
                }
            }
        }
        None
    }
    let list = match parent.and_then(|p| folder(&mut target.items, p)) {
        Some(f) => &mut f.items,
        None => &mut target.items,
    };
    let index = index.min(list.len());
    list.insert(index, item);
}

// ---------------------------------------------------------------------------
// Id translation
// ---------------------------------------------------------------------------

fn map_ids(c: &Collection, f: &mut dyn FnMut(&str) -> Id) -> Collection {
    fn walk(items: &mut [Item], f: &mut dyn FnMut(&str) -> Id) {
        for item in items {
            match item {
                Item::Folder(fo) => {
                    fo.id = f(&fo.id);
                    walk(&mut fo.items, f);
                }
                Item::Request(r) => r.id = f(&r.id),
            }
        }
    }
    let mut c = c.clone();
    walk(&mut c.items, f);
    c
}

fn to_parent_ids(fork: &Collection, link: &ForkLink) -> Collection {
    map_ids(fork, &mut |id| link.ids.get(id).cloned().unwrap_or_else(|| id.to_string()))
}

/// Back to fork ids; parent items new to the fork get fresh ids.
fn from_parent_ids(target: Collection, fork: &Collection, link: &mut ForkLink) -> Collection {
    let reverse: HashMap<Id, Id> = link.ids.iter().map(|(f, p)| (p.clone(), f.clone())).collect();
    let local: HashSet<Id> = flatten(fork).into_keys().collect();
    let mut added = Vec::new();
    let mut out = map_ids(&target, &mut |id| match reverse.get(id) {
        Some(f) => f.clone(),
        None if local.contains(id) => id.to_string(),
        None => {
            let fresh = new_id();
            added.push((fresh.clone(), id.to_string()));
            fresh
        }
    });
    link.ids.extend(added);
    // Drop mappings of items that no longer exist in the fork.
    let present: HashSet<Id> = flatten(&out).into_keys().collect();
    link.ids.retain(|f, _| present.contains(f));
    out.id = fork.id.clone();
    out.name = fork.name.clone();
    out
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

fn scrub_auth(auth: &mut Auth) {
    auth.scrub_secrets();
}

/// The request's auth and the auth of its examples' requests.
fn scrub_request(r: &mut RequestItem) {
    scrub_auth(r.request.auth_mut());
    for ex in &mut r.examples {
        if let Some(req) = &mut ex.request {
            scrub_auth(&mut req.auth);
        }
    }
}

fn scrub_vars(vars: &mut [Variable]) {
    for v in vars {
        if v.is_secret() {
            v.value.clear();
        }
        v.current = None;
    }
}

/// The collection with secret values blanked and current values dropped.
pub fn scrub(c: &Collection) -> Collection {
    fn walk(items: &mut [Item]) {
        for item in items {
            match item {
                Item::Folder(f) => {
                    scrub_auth(&mut f.auth);
                    scrub_vars(&mut f.variables);
                    walk(&mut f.items);
                }
                Item::Request(r) => scrub_request(r),
            }
        }
    }
    let mut c = c.clone();
    scrub_auth(&mut c.auth);
    scrub_vars(&mut c.variables);
    walk(&mut c.items);
    c
}

// ---------------------------------------------------------------------------
// Line diff for the review dialog
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    Same,
    Removed,
    Added,
}

/// Line diff (longest common subsequence) of two texts.
pub fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<(DiffOp, &'a str)> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (n, m) = (a.len(), b.len());
    // Texts here are single items, so the quadratic table stays small.
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::with_capacity(n.max(m)));
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((DiffOp::Same, a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push((DiffOp::Removed, a[i]));
            i += 1;
        } else {
            out.push((DiffOp::Added, b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| (DiffOp::Removed, *l)));
    out.extend(b[j..].iter().map(|l| (DiffOp::Added, *l)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::HttpRequest;

    fn sample() -> Collection {
        let mut c = Collection::new("API");
        let mut users = Folder::new("Users");
        let mut list = RequestItem::new("List", HttpRequest::new("GET", "{{baseUrl}}/users"));
        list.examples.push(crate::model::Example { name: "OK".into(), status: 200, ..Default::default() });
        users.items.push(list.into());
        users.items.push(RequestItem::new("Get", HttpRequest::new("GET", "{{baseUrl}}/users/:id")).into());
        c.items.push(users.into());
        let mut login = RequestItem::new("Login", HttpRequest::new("POST", "{{baseUrl}}/login"));
        *login.request.auth_mut() = Auth::bearer("s3cret");
        c.items.push(login.into());
        c
    }

    fn request_mut<'a>(c: &'a mut Collection, name: &str) -> &'a mut RequestItem {
        fn find<'a>(items: &'a mut [Item], name: &str) -> Option<&'a mut RequestItem> {
            items.iter_mut().find_map(|it| match it {
                Item::Request(r) if r.name == name => Some(r),
                Item::Folder(f) => find(&mut f.items, name),
                _ => None,
            })
        }
        find(&mut c.items, name).unwrap()
    }

    fn names(c: &Collection) -> Vec<String> {
        let mut v: Vec<String> = c.requests().iter().map(|r| r.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn fork_has_new_ids_and_no_secrets_in_base() {
        let parent = sample();
        let (fork, link) = create_fork(&parent, "API (fork)", 1);
        assert_ne!(fork.id, parent.id);
        let pids: HashSet<_> = parent.requests().iter().map(|r| r.id.clone()).collect();
        assert!(fork.requests().iter().all(|r| !pids.contains(&r.id)));
        assert_eq!(link.ids.len(), 4);
        assert!(!serde_json::to_string(&link.base).unwrap().contains("s3cret"));
        assert!(plan_pull(&link, &parent, &fork).is_empty());
        assert!(plan_merge(&link, &parent, &fork).is_empty());
    }

    #[test]
    fn pull_applies_parent_changes_and_keeps_fork_ids() {
        let mut parent = sample();
        let (mut fork, mut link) = create_fork(&parent, "Fork", 1);
        let list_id = request_mut(&mut fork, "List").id.clone();
        request_mut(&mut parent, "List").request.as_http_mut().unwrap().url = "{{baseUrl}}/v2/users".into();
        parent.items.push(RequestItem::new("Health", HttpRequest::new("GET", "{{baseUrl}}/health")).into());
        let plan = plan_pull(&link, &parent, &fork);
        let kinds: Vec<_> = plan.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert!(kinds.contains(&("List", ChangeKind::Modified)));
        assert!(kinds.contains(&("Health", ChangeKind::Added)));
        apply_pull(&mut link, &plan, &parent, &mut fork);
        let list = request_mut(&mut fork, "List");
        assert_eq!(list.id, list_id);
        assert_eq!(list.request.url(), "{{baseUrl}}/v2/users");
        let health = request_mut(&mut fork, "Health").id.clone();
        assert!(parent.requests().iter().all(|r| r.id != health), "new items get fork ids");
        assert_eq!(link.ids.get(&health), parent.requests().iter().find(|r| r.name == "Health").map(|r| &r.id));
        assert!(plan_pull(&link, &parent, &fork).is_empty());
    }

    #[test]
    fn conflicts_need_a_choice() {
        let mut parent = sample();
        let (mut fork, mut link) = create_fork(&parent, "Fork", 1);
        request_mut(&mut parent, "Get").request.as_http_mut().unwrap().method = "HEAD".into();
        request_mut(&mut fork, "Get").request.as_http_mut().unwrap().method = "OPTIONS".into();
        let mut plan = plan_pull(&link, &parent, &fork);
        assert_eq!(plan.conflicts(), 1);
        assert!(!plan.entries[0].apply);
        // Unresolved conflicts change nothing.
        apply_pull(&mut link.clone(), &plan, &parent, &mut fork.clone());
        plan.entries[0].apply = true;
        plan.entries[0].take_source = false;
        let mut kept = fork.clone();
        apply_pull(&mut link.clone(), &plan, &parent, &mut kept);
        assert_eq!(request_mut(&mut kept, "Get").request.as_http().unwrap().method, "OPTIONS");
        plan.entries[0].take_source = true;
        apply_pull(&mut link, &plan, &parent, &mut fork);
        assert_eq!(request_mut(&mut fork, "Get").request.as_http().unwrap().method, "HEAD");
    }

    #[test]
    fn merge_pushes_fork_changes_with_parent_ids() {
        let mut parent = sample();
        let (mut fork, mut link) = create_fork(&parent, "Fork", 1);
        let parent_ids: HashSet<_> = parent.requests().iter().map(|r| r.id.clone()).collect();
        request_mut(&mut fork, "Login").description = "Gets a token".into();
        let extra = RequestItem::new("Logout", HttpRequest::new("POST", "{{baseUrl}}/logout"));
        let extra_id = extra.id.clone();
        fork.items.push(extra.into());
        // Removed in the fork, unchanged in the parent: removed on merge.
        if let Item::Folder(f) = &mut fork.items[0] {
            f.items.retain(|i| i.name() != "Get");
        }
        let plan = plan_merge(&link, &parent, &fork);
        assert_eq!(plan.entries.len(), 3, "{plan:?}");
        apply_merge(&mut link, &plan, &fork, &mut parent);
        assert_eq!(names(&parent), vec!["List", "Login", "Logout"]);
        assert_eq!(request_mut(&mut parent, "Login").description, "Gets a token");
        let logout = request_mut(&mut parent, "Logout").id.clone();
        assert_ne!(logout, extra_id);
        assert!(!parent_ids.contains(&logout));
        assert_eq!(link.ids.get(&extra_id), Some(&logout));
        // The secret survived (merge works on real items, compares scrubbed ones).
        assert_eq!(request_mut(&mut parent, "Login").request.auth().get("token"), "s3cret");
        assert!(plan_merge(&link, &parent, &fork).is_empty());
        assert!(plan_pull(&link, &parent, &fork).is_empty());
    }

    #[test]
    fn secret_only_differences_are_ignored() {
        let parent = sample();
        let (mut fork, link) = create_fork(&parent, "Fork", 1);
        *request_mut(&mut fork, "Login").request.auth_mut() = Auth::bearer("other-token");
        assert!(plan_merge(&link, &parent, &fork).is_empty());
    }

    #[test]
    fn folder_rename_and_root_settings() {
        let mut parent = sample();
        let (mut fork, mut link) = create_fork(&parent, "Fork", 1);
        if let Item::Folder(f) = &mut parent.items[0] {
            f.name = "People".into();
        }
        parent.scripts.pre_request = "console.log(1)".into();
        let plan = plan_pull(&link, &parent, &fork);
        assert_eq!(plan.entries.len(), 2, "{plan:?}");
        apply_pull(&mut link, &plan, &parent, &mut fork);
        assert_eq!(fork.items[0].name(), "People");
        assert_eq!(fork.scripts.pre_request, "console.log(1)");
        assert_eq!(names(&fork), names(&parent), "folder children kept");
    }

    #[test]
    fn pull_keeps_the_parent_order_of_new_items() {
        let mut parent = sample();
        let (mut fork, mut link) = create_fork(&parent, "Fork", 1);
        for name in ["Charlie", "Bravo", "Alpha"] {
            parent.items.push(RequestItem::new(name, HttpRequest::new("GET", "{{baseUrl}}/x")).into());
        }
        let plan = plan_pull(&link, &parent, &fork);
        apply_pull(&mut link, &plan, &parent, &mut fork);
        let order = |c: &Collection| c.items.iter().map(|i| i.name().to_string()).collect::<Vec<_>>();
        assert_eq!(order(&fork), order(&parent));
    }

    #[test]
    fn example_auth_secrets_are_scrubbed_from_the_base() {
        let mut parent = sample();
        let login = request_mut(&mut parent, "Login");
        let req = login.request.as_http().unwrap().clone();
        login.examples.push(crate::model::Example { request: Some(req), ..Default::default() });
        let (fork, link) = create_fork(&parent, "Fork", 1);
        assert!(!serde_json::to_string(&link.base).unwrap().contains("s3cret"));
        assert!(plan_pull(&link, &parent, &fork).is_empty());
        assert!(plan_merge(&link, &parent, &fork).is_empty());
    }

    #[test]
    fn diff_lines() {
        let d = line_diff("a\nb\nc", "a\nx\nc");
        assert_eq!(d, vec![(DiffOp::Same, "a"), (DiffOp::Removed, "b"), (DiffOp::Added, "x"), (DiffOp::Same, "c")]);
    }
}
