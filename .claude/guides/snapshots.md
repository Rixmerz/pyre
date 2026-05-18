# Snapshots

## Model

Every snapshot is an **orphan commit** under `refs/jig/snapshots/<id>`. These refs do NOT appear in:
- `git tag -l`
- `git branch -a`
- `git log` (default view)
- IDE branch pickers

They only show via `git for-each-ref refs/jig/`. This means snapshots cannot pollute your normal git workflow.

## When snapshots fire

- `graph_activate` — baseline before a workflow runs
- `graph_traverse` — snapshot of the state leaving each phase
- `PostToolUse(Bash)` — after any non-readonly shell command (30s throttle)
- Manual via `snapshot_create(label="...", phase="...")`

## Operations

```
snapshot_list()                       → reverse-chronological listing
snapshot_diff(a, b)                   → git diff between two snapshots
snapshot_restore(id, dry_run=True)    → preview the revert
snapshot_restore(id, dry_run=False)   → perform (overwrites working tree)
```

Restore does not auto-commit. You decide whether to stage/commit the result.

## Journal

A local journal at `$PROJECT/.jig/snapshots.jsonl` is the authoritative index for jig. It contains ids, labels, phases, and the commit SHAs. If you delete the journal, the refs still exist and are valid; you just lose label metadata.

## Cleanup

```
snapshot_prune(keep=100)
```

Deletes oldest snapshots beyond `keep`. For one-off cleanup:
```bash
git for-each-ref refs/jig/snapshots/ | awk '{print $3}' | xargs -n1 git update-ref -d
```

## What snapshots DON'T do

- They don't replace commits. They're ephemeral "undo points", not history.
- They don't track uncommitted files via git stash semantics — they capture a full tree view including untracked files.
- They don't push to remote. `refs/jig/` is local-only unless you explicitly push with a refspec.
