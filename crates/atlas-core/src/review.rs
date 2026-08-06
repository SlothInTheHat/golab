//! Review: an approval gate between "I think I'm done" and "it's merged."
//!
//! `atlas task done` still works exactly as it always has — a direct
//! Running → Done transition, for anyone who doesn't want a gate. Review is
//! a parallel, opt-in path built entirely on the existing state machine: it
//! adds one `TaskState` variant and three thin wrappers around
//! `set_task_state`, reusing its assignee-guard, its lease release on
//! terminal states, and its dependency-request resolution — none of that
//! had to be rebuilt.

use anyhow::{anyhow, bail, Result};
use serde_json::json;

use crate::model::*;
use crate::protocol::NewRequest;
use crate::store::Store;
use crate::work::TaskView;

impl Store {
    /// Submit your own work for someone else to approve. Leases are kept —
    /// the work isn't merged yet, just declared ready.
    ///
    /// `set_task_state`'s generic assignee guard only fires on a *terminal*
    /// transition (Done/Failed), so it does nothing for Review — this method
    /// carries its own check instead, since only the person doing the work
    /// should get to declare it ready.
    pub fn submit_for_review(&mut self, task_id: &str, agent: &str) -> Result<TaskView> {
        let t = self
            .task(task_id)?
            .ok_or_else(|| anyhow!("no such task: {task_id}"))?;
        if t.task.assignee.as_deref() != Some(agent) {
            bail!(
                "{task_id} is assigned to {}, not {agent}",
                t.task.assignee.as_deref().unwrap_or("nobody")
            );
        }
        self.set_task_state(task_id, TaskState::Review, Some(agent), None, true)
    }

    /// Approve it: this is where the lease release and dependency-request
    /// resolution actually happen, identical to what a direct `task done`
    /// has always done. By default anyone but the submitter may approve —
    /// mirroring the existing rule that a request's own opener cannot answer
    /// it — and `force` lets a human override that, including self-approval.
    ///
    /// This is the reverse of `set_task_state`'s own default (only the
    /// assignee may close their own task), which is correct: that guard
    /// exists to stop one agent unilaterally closing another's work, but
    /// review approval is *supposed* to come from someone other than the
    /// assignee. The check above is this method's real gate, so it always
    /// passes `force: true` to `set_task_state` beneath it.
    pub fn approve_review(&mut self, task_id: &str, agent: &str, force: bool) -> Result<TaskView> {
        if !force {
            if let Some(t) = self.task(task_id)? {
                if t.task.assignee.as_deref() == Some(agent) {
                    bail!(
                        "{agent} submitted {task_id}; someone else needs to approve it \
                         (pass --force to self-approve)"
                    );
                }
            }
        }
        self.set_task_state(task_id, TaskState::Done, Some(agent), None, true)
    }

    /// Send it back. Reopens the task as `Running` under its original
    /// assignee (who keeps their leases throughout — nothing was ever
    /// released) and notifies them why, using the same request/response
    /// channel agents already negotiate through.
    pub fn reject_review(
        &mut self,
        task_id: &str,
        agent: &str,
        reason: Option<&str>,
    ) -> Result<TaskView> {
        let before = self
            .task(task_id)?
            .ok_or_else(|| anyhow!("no such task: {task_id}"))?;
        let assignee = before.task.assignee.clone();

        let after = self.set_task_state(task_id, TaskState::Running, Some(agent), reason, true)?;

        if let Some(assignee) = assignee.filter(|a| a != agent) {
            self.open_request(&NewRequest {
                to: Some(assignee),
                body: json!({ "task": task_id, "reason": reason }),
                deadline_secs: Some(3600),
                ..NewRequest::new(
                    request_kind::REVIEW,
                    agent,
                    &format!("{task_id} sent back for changes"),
                )
            })?;
        }
        Ok(after)
    }

    pub fn tasks_in_review(&self) -> Result<Vec<TaskView>> {
        Ok(self
            .tasks()?
            .into_iter()
            .filter(|t| t.task.state == TaskState::Review)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Direction;

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/pay.ts"),
            "export function charge(x: number) { return x; }\n",
        )
        .unwrap();
        let mut store = Store::init(dir.path()).unwrap();
        crate::scan::scan(&mut store, crate::ids::DEFAULT_REPO_ID, dir.path(), &[], false).unwrap();
        store.register_agent("author", "claude").unwrap();
        store.register_agent("reviewer", "claude").unwrap();
        (dir, store)
    }

    #[test]
    fn submitting_for_review_keeps_leases_held() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.claim_next("author", 300).unwrap().unwrap();

        let submitted = store.submit_for_review(&t.id, "author").unwrap();
        assert_eq!(submitted.task.state, TaskState::Review);
        assert_eq!(
            store.active_leases(None).unwrap().len(),
            1,
            "leases must not be released on submission — the work isn't merged"
        );

        // And it is still visible in the plan, not vanished.
        let plan = store.plan().unwrap();
        assert_eq!(plan.in_review.len(), 1);
        assert_eq!(plan.in_review[0].task.task.id, t.id);
    }

    #[test]
    fn approving_a_review_releases_leases_and_resolves_dependencies() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.claim_next("author", 300).unwrap().unwrap();
        store.submit_for_review(&t.id, "author").unwrap();

        // Someone else is blocked on this task finishing.
        store
            .open_request(&NewRequest {
                resource_task: Some(t.id.clone()),
                ..NewRequest::new(request_kind::DEPENDENCY, "waiter", "blocked on the fix")
            })
            .unwrap();

        let approved = store.approve_review(&t.id, "reviewer", false).unwrap();
        assert_eq!(approved.task.state, TaskState::Done);
        assert!(store.active_leases(None).unwrap().is_empty());

        let waiting = store
            .requests(Some("waiter"), Direction::Outbox, false)
            .unwrap();
        assert_eq!(waiting[0].state, RequestState::Fulfilled, "dependency resolves on full approval");
    }

    #[test]
    fn a_review_alone_does_not_resolve_dependency_requests() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.reassign_task(&t.id, "author", None).unwrap();
        store
            .open_request(&NewRequest {
                resource_task: Some(t.id.clone()),
                ..NewRequest::new(request_kind::DEPENDENCY, "waiter", "blocked on the fix")
            })
            .unwrap();

        store.submit_for_review(&t.id, "author").unwrap();
        let waiting = store
            .requests(Some("waiter"), Direction::Outbox, false)
            .unwrap();
        assert_eq!(
            waiting[0].state,
            RequestState::Open,
            "submission for review is not completion"
        );
    }

    #[test]
    fn only_the_assignee_can_submit_their_own_work() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.reassign_task(&t.id, "author", None).unwrap();

        let err = store.submit_for_review(&t.id, "reviewer").unwrap_err();
        assert!(err.to_string().contains("assigned to author"), "{err}");
        assert_eq!(store.task(&t.id).unwrap().unwrap().task.state, TaskState::Running);

        assert!(store.submit_for_review(&t.id, "author").is_ok());
    }

    #[test]
    fn the_submitter_cannot_approve_their_own_work_without_force() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.reassign_task(&t.id, "author", None).unwrap();
        store.submit_for_review(&t.id, "author").unwrap();

        let err = store.approve_review(&t.id, "author", false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(store.task(&t.id).unwrap().unwrap().task.state, TaskState::Review);

        assert!(store.approve_review(&t.id, "author", true).is_ok());
    }

    #[test]
    fn rejecting_reopens_the_task_and_notifies_the_assignee() {
        let (_d, mut store) = fixture();
        let t = store.add_task("fix charge", 5, &[]).unwrap();
        store.set_task_scope(&t.id, &["charge".to_string()]).unwrap();
        store.claim_next("author", 300).unwrap().unwrap();
        store.submit_for_review(&t.id, "author").unwrap();

        let rejected = store
            .reject_review(&t.id, "reviewer", Some("missing a null check"))
            .unwrap();
        assert_eq!(rejected.task.state, TaskState::Running);
        assert_eq!(rejected.task.assignee.as_deref(), Some("author"));
        assert_eq!(
            store.active_leases(None).unwrap().len(),
            1,
            "the assignee keeps their leases through a rejection"
        );

        let inbox = store.requests(Some("author"), Direction::Inbox, true).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, request_kind::REVIEW);
        assert_eq!(inbox[0].body["reason"], "missing a null check");
    }

    #[test]
    fn tasks_in_review_lists_only_review_state_tasks() {
        let (_d, mut store) = fixture();
        let a = store.add_task("a", 5, &[]).unwrap();
        store.add_task("b", 3, &[]).unwrap();
        store.reassign_task(&a.id, "author", None).unwrap();
        store.submit_for_review(&a.id, "author").unwrap();

        let listed = store.tasks_in_review().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].task.id, a.id);
    }
}
