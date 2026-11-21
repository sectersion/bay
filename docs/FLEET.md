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
- Files work packages **only** from owner-approved milestones (M1–M4 =
  #2–#5). Each package is a pre-approved GitHub issue that states its exit
  test up front — see M1's boxes #14–#17, each marked "Pre-approved box".
- No package without an exit test; no package outside an approved milestone.

### Worker
- Claims work from the polymer board, never from thin air: **workers do not
  self-generate scope.** Found new work while implementing? Comment it on the
  issue you're working; triage decides whether it becomes one.
- One open PR at a time. A worker stops at "ready for review": push, PR with
  `Closes #N`, reviews requested. Never merge.

### Reviewers
- Two independent seats per PR, different postures:
  - **verifier of claims** — checks every claim in the PR body against
    primary evidence (logs, measurements re-read at source, not folklore).
  - **adversary** — tries to break it: exit-test loopholes, edge cases,
    diff outside issue scope.
- **No VM launches except the designated live-repro seat.** Cloud
  Hypervisor locks its disk image, so a second VM on the same image fails
  the lock — M0's first 50M measurement run was lost exactly this way
  (#12 review). At most one seat per PR may boot anything, and only when
  static verification can't settle a claim.
- Review rounds: after **3 rounds** of CHANGES_REQUESTED without
  convergence, add the `needs-human` label and stop iterating. (Create the
  label on first use if triage hasn't.)

### Merge queue
- **One agent**, sole holder of merge rights. Merges only when all three
  hold, re-checked at head:
  1. two `APPROVED` reviews naming the current head SHA,
  2. CI green (three-OS `cargo test` matrix, #10),
  3. diff stays within the issue's scope.
- Squash-merge only (repo-enforced).
- Post-merge breakage: **revert immediately**, then file a bug issue with
  the failure evidence. Never debug on main.

## Polymer board

- **Identity:** each worker registers via `register_subagent` (name + role);
  the server parents it to the caller, so board activity is attributable per
  worker. Don't share one identity.
- **Heartbeat:** `send_heartbeat` refreshes *agent liveness* (`last_seen`)
  only — it does **not** renew your task lease. Set
  `claim_task`'s `lease_duration_seconds` above the worst-case task time.
  Renewing your own live lease keeps the generation; letting it expire starts
  a new one and you can lose the task.
- **Status flow:** `to_do` → claim → `in_progress` → `done` (merge queue) or
  `failed` (stuck, with a comment saying why). The board has no `in_review`
  status; review state lives on the PR.
- **@mentions:** a typo'd or wrong-case `@agent_name` notifies nobody —
  silent drop. Always assert on the returned mentions array; if the agent
  you meant isn't listed, fix the name and repost.

## WSL2 constraints

The Windows host runs the Linux engine inside WSL2 (`ARCHITECTURE.md`). Paid
for in M0 (#12 spike notes), still true:

- **Idle teardown:** mounts, sockets, and VM processes died between bare
  `wsl.exe` invocations (~8s idle). Covered on this host: `/etc/wsl.conf`
  sets `[boot] systemd=true` and the polymer server runs as
  `polymer.service` (Restart=always), keeping the distro up. Don't rely on
  it for correctness — the next constraint is the one that bites.
- **One WSL session per scenario:** run a whole scenario inside a single
  `wsl.exe … bash <script>` invocation, never across separate ones.
- **Artifacts on ext4, never `/mnt/c`:** VM images, sockets, and snapshots
  live under the distro's own filesystem (`/root/…`).
- **`wsl.exe` mangles quoted `-c` scripts** (and POSIX paths get rewritten
  en route from Git Bash). Write scripts to a file and execute the file; for
  anything copied out of `/mnt/c`, `tr -d '\r'` first — CRLF from Windows
  checkouts is what bit the M0 repro steps.
- **`pkill -f` matches your own `bash -c`** — you kill yourself. Use
  `killall` on comm names.

## Hard stop & morning digest

- **Milestone hard stop:** when the claimed milestone's board queue is
  empty — or the next action would be new scope — stop. No self-generated
  work overnight, ever; that is what triage and the owner's milestone
  approval exist to prevent.
- **Morning digest** — the owner's only read surface, one comment listing:
  - **merged** — PRs merged, at which head SHAs;
  - **blocked** — tasks past lease still `in_progress`, or `failed`, with why;
  - **reverted** — any reverts plus the bug issues filed for them;
  - **needs-human** — everything labeled after the 3-round cap.
