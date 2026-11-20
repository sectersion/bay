# bay

Give each agent its own bay.

Disposable microVM homes for coding agent fleets. `bay` boots a tiny Linux VM
(snapshot-restored in ~200ms), clones your repo into it, and runs a fleet of
agents — Claude Code, Codex, anything — where none of them ever reach each
other. No shared `~/.claude`, no shared `.git`, no port collisions, no root on
your laptop.

Worktrees were supposed to do this. They share too much.

## The idea

```
bay bake <manifest>          # golden snapshot: tools, dotfiles, agent CLIs
bay up 4 --repo .            # N disposable bays, CoW clones of the snapshot
bay exec 2 -- claude "..."   # PTY pass-through into a bay
bay halt 3 / wake 3          # park it: zero RAM, wakes in place
bay kill 4 / kill --all      # it never touched anything else
```

The snapshot layer is the undo button: a rogue agent reverts to its exact
pre-crash state, and "two agents, same starting point, different prompts"
falls out for free. Parked agents cost nothing, so fleet size isn't bounded by
RAM.

## Status

Nothing built yet. Stack and platform target are open decisions.
