# Worktrees: one tree per agent

Two agents editing the same file on the same branch is not a merge conflict you get to
resolve. It is one agent's work silently overwritten by the other, usually discovered an hour
later. Git has had the fix for a decade:

```sh
horde spawn --cmd claude --name ads --worktree
horde spawn --cmd claude --name ops --worktree
```

Each agent gets its own working tree on its own branch, and starts there. They can both run
the full test suite, both rewrite `src/main.rs`, and neither can touch what the other is
holding.

```
~/dev/WCP        main          you
~/dev/WCP-ads    horde/ads     ads
~/dev/WCP-ops    horde/ops     ops
```

`--worktree` takes an optional name, defaulting to the agent's own:

```sh
horde spawn --cmd claude --name kenny --worktree            # ~/dev/WCP-kenny,  horde/kenny
horde spawn --cmd claude --name kenny --worktree hotfix     # ~/dev/WCP-hotfix, horde/hotfix
```

It is **opt-in**, and agents are told to keep it that way: a worktree is a directory on your
disk and a branch in your repository, and neither is an agent's to create uninvited. Ask for
one — "each in its own worktree" — and a lead agent building a fleet will pass the flag.

## Where they live, and why

Beside the project, named after it: `<project>-<agent>`, a sibling of the repository.

This is the opposite of where horde started, which was `<repo>/.horde/worktrees/<name>`.
Nesting them inside the repository meant three problems, all of which the sibling layout simply
does not have:

- **The main tree could see them.** `git status` reported `?? .horde/`, so horde had to write
  `.horde/` into `.git/info/exclude` on every repository it touched. A sibling is not in the
  repository, so there is nothing to hide.
- **`git clean -ffdx` removed them.** Git protects nested repositories at every lesser force
  level, but that one takes them. Nothing outside the repository is in reach of any `git clean`.
- **Agents could wander into them.** A search in the main tree could recurse into a worktree and
  return one hit per agent per match. The old layout leaned on the leading dot in `.horde/` to
  stop that; a sibling needs no trick.

And it is the layout you can *see*. Worktrees are where the work is, and work you cannot find
in your editor's file list may as well not exist.

The one thing the old placement did better: it kept everything under one directory you could
delete in a single stroke. `horde worktree list` is the replacement for that.

## Which trees are horde's

The branch, not the path.

Everything horde creates is on `horde/<name>`. A worktree you made yourself is never listed and
never removable — wherever you put it, and whatever you called the directory. You can park one
right next to horde's, named exactly as horde would name it, and it is still yours:

```sh
git worktree add ../WCP-mine -b mine
horde worktree list          # does not mention it
horde worktree remove mine   # no worktree called mine
```

This is also why **worktrees an older horde nested inside the repository keep working**. They
carry the same `horde/` branch, so they are still listed, still removable by name, and asking
for the same agent again resumes the tree it is already in rather than trying to check the
branch out twice. Nothing needs migrating; move them yourself if you want them beside the
project, and horde will follow them there.

## Managing them

```sh
horde worktree list                 # every one horde made, and who is in it
horde worktree remove ads           # tidy up
horde worktree remove ads --force
```

Worktrees **survive a closed pane**, deliberately. Nothing an agent produced should be lost
by closing a window, which is the failure that actually costs you something. `remove` is the
only thing that deletes one, and it refuses twice over: once if a live pane is still working
in it, and once if the tree has uncommitted changes, unless you say `--force`.

Removing a worktree keeps its branch, because the branch may hold commits. horde prints the
command to drop that too.

## Re-running is resuming

`--worktree` for a name that already has one hands back the existing tree rather than
failing, with whatever work is in it. A pane that closed and is being replaced lands back
where it was, across a daemon restart or an upgrade.

That lookup is by branch rather than by directory, so it finds the tree even if you have moved
it since.

## Where a branch shows up

| Where | What it shows |
|---|---|
| sidebar, SPACES row | the project's own branch, with `*` when the main tree is dirty |
| pane title | the agent's branch, **only** for a pane horde put in a worktree |
| roster (`ctrl+b o`) | the same, next to the agent's role |
| `horde worktree list` | all of them: where each one is, who is in it, and which have uncommitted work |

The pane title deliberately says nothing for an ordinary pane. Every pane not in a worktree
is on the project's branch, which the sidebar already says once for the whole project;
repeating it on six pane titles would be six copies of one fact, and would bury the one case
where the answer differs per pane.

"Dirty" means tracked files differ from `HEAD`. Untracked files do not count, or a
`node_modules` and a dev server's build output would leave every project permanently marked.
