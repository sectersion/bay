# FLEET.md — the overnight fleet contract

Operational contract for autonomous multi-agent development on bay while the
owner sleeps. Single source of fleet truth; GitHub issues stay the global
todo list. This codifies what M0 (#1, #12, #13) proved by hand. The three
rules in `CLAUDE.md` § "Repo rules" are absolute and this file never
overrides them.

## Repo rules

Not restated here — see `CLAUDE.md` § "Repo rules": every change gets an
issue and its own PR, two independent agent reviews must pass before any
merge (each `APPROVED` names the head SHA it reviewed; any push invalidates
both), never push main, squash-only. Everything below is operating detail on
top of those.

## Roles

### Triage
- **Scope provenance is the `pre-approved` GitHub label.** The label marks
  the owner's awake intent: it is applied only under an explicit owner
  directive, and no overnight agent applies, removes, **or edits the body of
  a labeled issue** — even though it shares the owner's credentials; the
  label proves the approved body *as written*, and relaboring by rewriting
  an exit test is scope theft even when the label survives. Fleet may only build issues carrying it
  (M1's boxes #14–#17). Milestones (M1–M4 = #2–#5) say *where* work
  belongs; the label is the *proof* it was approved. A body string like
  "Pre-approved box" is not provenance — any agent can type it.
- Overnight triage may draft new issues **into `needs-human` only** — an
  agent-filed, agent-labeled, or unlabeled issue is never built. No package
  without an exit test.

### Worker
- Claims work from the polymer board, never from thin air: **workers do not
  self-generate scope; workers claim only `pre-approved`-labeled boxes.**
  Found new work while implementing? Comment it on the issue you're
  working; triage decides whether it becomes one.
- One open PR at a time. A worker stops at "ready for review": push, PR with
  `Closes #N`, reviews requested. Never merge.

### Reviewers
- Two independent seats per PR, different postures:
  - **verifier of claims** — checks every claim in the PR body against
    primary evidence (logs, measurements re-read at source, not folklore).
  - **adversary** — tries to break it: exit-test loopholes, edge cases,
    diff outside scope (see merge gate 3: anything the exit test doesn't
    need).
- **Live repro only when static verification can't settle a claim, and only
  via the vm-seat (see "One VM at a time" below).** The seat is fleet-wide,
  not per-PR — a worker booting its exit test blocks reviewers on every PR.
- Review rounds: after **3 rounds** of CHANGES_REQUESTED without
  convergence, add the `needs-human` label and stop iterating. (Create the
  label on first use if triage hasn't.)

### Merge queue
- **One agent**, sole holder of merge rights. Merges only when all three
  hold, re-checked at head:
  1. two `APPROVED` reviews naming the current head SHA — **each review
     body names its registered board seat and posture, and the two names
     must be two distinct entries on the agent roster.** All fleet GitHub
     action shares one account; the roster cross-check is a tripwire that
     catches one seat double-posting — it cannot prove independence (one
     shared account, self-attested seats); independence stays procedural,
     like the gate in CLAUDE.md rule 2. The digest lists the two seat names.
  2. CI green (three-OS `cargo test` matrix, #10),
  3. **Scope = the minimal change set that makes the issue's exit test
     pass.** If you can't name a change's contribution to the exit test,
     it belongs in a comment on the issue, not in the diff.
- Squash-merge only (repo-enforced).
- Post-merge breakage: file the bug issue with the failure evidence and
  open a revert PR (`git revert`) immediately; it merges through the normal
  gates and may be flagged for priority. **Nobody force-pushes, ever; main
  is never debugged.** main is push-protected with force-push disabled —
  there is no emergency path that skips the gates.

## One VM at a time (vm-seat)

- **Any agent that must boot a VM — a worker running its package's exit
  test or a reviewer doing a live repro — first claims the singleton
  `vm-seat` board task:** title exactly `fleet:vm-seat`, created once via
  `create_task`, lease eternal — re-claim it at each boot with
  `lease_duration_seconds` above the worst-case boot+test.
- Cloud Hypervisor locks its disk image, so a second VM on the same golden
  fails the lock — M0's first 50M measurement run was lost exactly this way
  (#12 review), and qcow2 backing chains (#16) would make the collision
  routine, not exotic. A disk-lock failure means the seat is taken, not a
  bug to fight.
- The lock is the sharpest edge, not the only one: the seat serializes one
  VM at a time on the single WSL host for capacity and idle-teardown
  hygiene too — per-bay CoW overlays (M1) won't collide on the golden, but
  the claim-first rule stays unconditional either way.
- No extra tie-break machinery: `claim_task` on one task *is* the tie-break
  for two simultaneous repro needs. The seat holder posts repro output to
  the PR so static-only seats can consume it.

## Polymer board

- **Identity:** each worker registers via `register_subagent` (name + role);
  the server parents it to the caller, so board activity is attributable per
  worker. Don't share one identity.
- **Heartbeat:** `send_heartbeat` refreshes *agent liveness* (`last_seen`)
  only — it does **not** renew your task lease. Set
  `claim_task`'s `lease_duration_seconds` above the worst-case task time.
  Renewing your own live lease keeps the generation; letting it expire starts
  a new one and you can lose the task.
- **Status flow:** `to_do` → claim → `in_progress` → `done` or `failed`
  (stuck, with a comment saying why). **Workers never set `done`; only the
  merge-queue agent does, and only after squash-merge** — `done` is
  terminal, and premature `done` hides unmerged work (incident: a worker
  marked a task done before its PR #21 was reviewed/merged and the revert
  was correctly refused). The board has no `in_review` status; review
  state lives on the PR.
- **@mentions:** a typo'd or wrong-case `@agent_name` notifies nobody —
  silent drop. Always assert on the returned mentions array; if the agent
  you meant isn't listed, fix the name and repost.

## WSL2 constraints

The Windows host runs the Linux engine inside WSL2 (`ARCHITECTURE.md`).
Still true on this host; paid for in M0 (#12 spike notes) except where a
bullet says otherwise:

- **Idle teardown:** mounts, sockets, and VM processes died between bare
  `wsl.exe` invocations (~8s idle). Covered on this host: `/etc/wsl.conf`
  sets `[boot] systemd=true` and the polymer server runs as
  `polymer.service` (Restart=always), keeping the distro up. Don't rely on
  it for correctness — the next constraint is the one that bites.
- **One WSL session per scenario:** run a whole scenario inside a single
  `wsl.exe … bash <script>` invocation, never across separate ones.
- **Artifacts on ext4, never `/mnt/c`:** VM images, sockets, and snapshots
  live under the distro's own filesystem (`/root/…`).
- **Quoting/paths across `wsl.exe` are hostile from Git Bash:** Git Bash
  rewrites POSIX arguments en route (reproduced on this host during
  review; *not* a logged M0 incident). Write scripts to a file and execute
  the file; for anything copied out of `/mnt/c`, `tr -d '\r'` first — CRLF
  from Windows checkouts *is* a logged M0 incident (the repro steps).
- **`pkill -f` matches your own `bash -c`** — you kill yourself. Use
  `killall` on comm names.

## Hard stop & morning digest

- **Milestone hard stop:** when the claimed milestone's board queue is
  empty — or the next action would be new scope — stop. No self-generated
  work overnight, ever; that is what triage and the owner's milestone
  approval exist to prevent.
- **Morning digest** — the owner's only read surface, posted **by the
  merge-queue agent** as a comment on the milestone tracking epic (`#2` for
  M1) at the hard stop (empty board queue), or early when the PR set goes
  quiet. An early digest is not terminal: if work resumes and anything new
  merges, blocks, or reverts after publication, the merge queue amends the
  same digest — the owner's read surface never omits late-landed work.
  A no-op night still gets a digest saying so — silence is not a pass. One comment listing:
  - **merged** — PRs merged, at which head SHAs, naming the two review
    seats whose approvals cleared the roster cross-check;
  - **blocked** — tasks past lease still `in_progress`, or `failed`, with why;
  - **reverted** — any reverts plus the bug issues filed for them;
  - **needs-human** — everything labeled after the 3-round cap.
