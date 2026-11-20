# bay — repo rules

1. **Every feature gets an issue and its own PR.** GitHub issues are the
   global todo. Never put TODOs, checklists, or roadmap content in files —
   file an issue.
2. **Two independent agent reviews must pass before any merge.** Each posts a
   formal review comment on the PR saying APPROVED; if even one thing is
   wrong: fix, push, re-request both reviews. (GitHub rejects approving
   reviews from the author's own account, so enforcement here is procedural,
   not settings-based.)
3. **Never push to main.** Branch → PR → squash-merge. Main is protected,
   admins included.

Working notes:

- `cargo test` is the build gate.
- `docs/ARCHITECTURE.md` is the design; milestones are issues #1–#5,
  backlog #6. The roadmap is *not* a file.
- Fleet logic may touch a hypervisor only through the `Engine` trait in
  `src/engine.rs`.
