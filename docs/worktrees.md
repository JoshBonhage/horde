# Worktrees: one tree per agent

Two agents editing the same file on the same branch is not a merge conflict you get to
resolve. It is one agent's work silently overwritten by the other, usually discovered an hour
later. Git has had the fix for a decade:

```sh
horde spawn --cmd claude --name builder  --worktree
horde spawn --cmd claude --name reviewer --worktree
```

Each agent gets its own working tree on its own branch, and starts there. They can both run
the full test suite, both rewrite `src/main.rs`, and neither can touch what the other is
holding.

```
horde/builder    .horde/worktrees/builder    builder
horde/reviewer   .horde/worktrees/reviewer   reviewer
main             the repository itself       you
```

`--worktree` takes an optional name, defaulting to the agent's own:

```sh
horde spawn --cmd claude --name kenny --worktree            # branch horde/kenny
horde spawn --cmd claude --name kenny --worktree hotfix     # branch horde/hotfix
```

## Where they live, and why

`<repo>/.horde/worktrees/<name>`, with `.horde/` written to `.git/info/exclude`.

Both halves of that were checked rather than assumed, and both are load-bearing.

**`info/exclude`, not `.gitignore`.** The exclude file is per-clone and is not itself tracked,
so horde writes it without modifying a single file the repository owns. That matters most in
repositories you do not control. Without it every agent in the main tree sees `?? .horde/`,
and the first one to run `git add -A` commits a mess.

```
git status --short          →  ?? .horde/
echo ".horde/" >> .git/info/exclude
git status --short          →  (empty)
```

**The leading dot.** Agent search tools skip dot-directories by default, so `.horde/` is
invisible to a `rg` in the main tree. Without the dot, every search in the main tree would
return one hit per worktree per match:

```
rg --files  (as .horde/)             →  a.txt
rg --files  (as visible-worktrees/)  →  visible-worktrees/worktrees/builder/a.txt
                                        a.txt
```

The dot is doing that work, not the nesting.

**The one hazard this keeps** is `git clean -ffdx`, which removes nested repositories. Git
protects them at every lesser force level, so it takes a deliberately violent reset:

```
git clean -fdn    →  nothing
git clean -fdxn   →  Would skip repository .horde/worktrees/builder
git clean -ffdxn  →  Would remove .horde/
```

## Managing them

```sh
horde worktree list                 # every one horde made, and who is in it
horde worktree remove builder       # tidy up
horde worktree remove builder --force
```

Worktrees **survive a closed pane**, deliberately. Nothing an agent produced should be lost
by closing a window, which is the failure that actually costs you something. `remove` is the
only thing that deletes one, and it refuses twice over: once if a live pane is still working
in it, and once if the tree has uncommitted changes, unless you say `--force`.

Removing a worktree keeps its branch, because the branch may hold commits. horde prints the
command to drop that too.

Only worktrees under `.horde/worktrees/` are listed or removable. One you made yourself
somewhere else is yours, and horde will not offer to delete it.

## Re-running is resuming

`--worktree` for a name that already has one hands back the existing tree rather than
failing, with whatever work is in it. A pane that closed and is being replaced lands back
where it was, across a daemon restart or an upgrade.

## Where a branch shows up

| Where | What it shows |
|---|---|
| sidebar, SPACES row | the project's own branch, with `*` when the main tree is dirty |
| pane title | the agent's branch, **only** for a pane horde put in a worktree |
| roster (`ctrl+b o`) | the same, next to the agent's role |
| `horde worktree list` | all of them, with who is in each and which have uncommitted work |

The pane title deliberately says nothing for an ordinary pane. Every pane not in a worktree
is on the project's branch, which the sidebar already says once for the whole project;
repeating it on six pane titles would be six copies of one fact, and would bury the one case
where the answer differs per pane.

"Dirty" means tracked files differ from `HEAD`. Untracked files do not count, or a
`node_modules` and a dev server's build output would leave every project permanently marked.
