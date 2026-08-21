# The kanban

Your own board. **`ctrl+b T`**, or **`ctrl+space k`**.

horde has two boards and they are not the same board.

The **task board** ([orchestration](orchestration.md#8-the-shared-task-board)) is work agents
pull from. Every rule on it is written for them: claiming is a compare-and-set so two agents
cannot take one task, an open task stops being offered after a day, and a crashed agent's work
goes back in the pool.

The **kanban** is yours. It has columns you named, cards with due dates and descriptions and a
comment thread, and nothing on it is claimable by anybody. Work sits there for a month without
that meaning something has gone wrong.

They meet at one seam, and only when you ask — see [Handing a card to the
agents](#handing-a-card-to-the-agents).

## The board

```
 KANBAN   horde                                                    2 due

 BACKLOG 2                      TODO 2                       DOING 1
 ────────────────────────────── ──────────────────────────── ──────────────
 ╭ #1 ────────────────────────╮ ╭ #3 ──────────────────────╮ ╭ #5 ────────╮
 │ read up on the CSV spec    │ │ wire up the importer so  │ │ fix auth   │
 │                            │ │ it reads in chunks       │ │            │
 │                            │ │ in 2d #api #p1  ⚑ armed  │ │ ⚑ agents 💬│
 ╰────────────────────────────╯ ╰──────────────────────────╯ ╰────────────╯
 ╭ #2 ────────────────────────╮ ╭ #4 ──────────────────────╮
 │ someday: rewrite the parser│ │ chase the vendor about   │
 │                            │ │ the encoding             │
 │                            │ │ yesterday #vendor        │
 ╰────────────────────────────╯ ╰──────────────────────────╯

 hjkl move  HL shove  JK reorder  n new  enter open  / filter  p project  v list
```

| Key | Action |
|---|---|
| `h` `j` `k` `l` | move the cursor — or the arrow keys |
| `H` `L` | shove the card into the column left or right, at the end of it |
| `J` `K` | move the card up or down within its column |
| `n` | new card, in the column the cursor is in |
| `enter` | open the card |
| `/` | filter — matches titles, descriptions and tags, as you type |
| `p` | this project only / every project |
| `x` | show archived cards |
| `X` | archive the card, or bring it back |
| `v` or `tab` | the list, and back |
| `C` `R` `D` | new column · rename this one · remove this one |
| `<` `>` | move this column left or right |
| `g` `G` | top and bottom of the column |
| `r` | ask the daemon for the board again |
| `esc` or `q` | back to the terminal |

Escape out of the filter and it clears — a half-typed filter you abandoned is not a filter you
wanted.

### The mouse

| Gesture | Action |
|---|---|
| drag a card | move it, between columns or within one |
| double-click a card | open it |
| click a column | put the cursor in it |
| double-click a column's name | rename it |
| wheel | scroll the column under the pointer |

Dropping a card **above** a card puts it before that one; **below the last** card puts it at
the end; letting go where you picked it up does nothing at all.

A drag that began on a card belongs to that card until you let go, wherever the pointer
wanders — the same rule panes have.

Shift-drag is left alone deliberately: in most terminals that is how you select text, and
taking it would cost you the selection for nothing.

## A card

`enter` on a card opens it.

```
 #12  wire up the importer
 Todo  ·  added 2d ago

   due      2026-08-18 · in 2d
   tags     #api #p1
   project  horde
   agents   hand over when due within 3d

 DESCRIPTION
   Read the CSV in chunks so a 200MB file doesn't blow the heap.

   Ask the vendor about the encoding.

 COMMENTS  2
   josh@joshmacbook   2h ago
    parked until the schema lands

   builder   10m ago
    schema landed, picking this up
```

| Key | Field |
|---|---|
| `r` | the title |
| `e` | the description |
| `d` | the due date |
| `t` | the tags |
| `p` | the project |
| `a` | the agent arrangement |
| `c` | write a comment |
| `tab` | move between them · `enter` edits whatever the cursor is on |
| `A` | hand this card to the agents **now** |
| `X` | archive it |
| `j` `k` | scroll · `esc` back to the board |

Typing types. **`esc` saves, `ctrl+c` throws it away.** In a description `enter` is a new line,
which is why it cannot be the key that saves.

An empty value clears the field, the same contract every other text prompt in horde has.

### Due dates

The date box reads what you would actually write:

| You type | It means |
|---|---|
| `2026-08-20`, `2026/08/20` | that day |
| `08-20` | that day this year |
| `today`, `tomorrow`, `yesterday` | that day |
| `friday`, `mon` | the next one of those — never today |
| `+3d`, `3d`, `2w` | that far out |
| *nothing* | no due date |

Anything it cannot read is refused and the box stays open with what you typed still in it — a
typo must not quietly clear the date you were setting.

Dates are stored at local noon, so neither a timezone change nor a daylight-saving boundary
can move one onto the day before.

### Comments

Every comment carries who wrote it and when. Yours are signed `user@host` —
`josh@joshmacbook` — and an agent's are signed with the agent's own name. horde signs its own
as `horde`.

Set `kanban.author` in `config.toml` if your machine's real hostname is uglier than what you
want on your own notes.

## Handing a card to the agents

A card can be **armed**: hand this to the agents when its due date gets close.

Press `a` on a card and give it a window — `2d`, `12h`. Nothing happens until the due date is
that close. Then horde puts a real task on the agents' board, scoped to the card's project,
and writes on the card that it did:

```
 COMMENTS  3
   horde   just now
    handed to the agents as task #47

   builder   12m ago
    done — chunked reader, tests green
```

Whatever the agent reports comes home as a comment in the agent's name. **The card does not
move.** Deciding a thing is finished is the part you wanted a board for.

`A` hands a card over immediately, without waiting for the window.

Three things it will refuse rather than guess at:

- **A card with no due date** can never fire, and the card says `needs a due date` instead of
  sitting there looking handled.
- **A card with no project** is never handed over. An agent has to be told which tree to work
  in — this is the same failure the task board's own scoping exists to prevent.
- **`agents.board = false`** turns this off with everything else that puts work on that board.
  One switch, honoured everywhere.

A card is handed over once. If the agent gives the task back rather than finishing it, the
link is released and the card can be armed again.

### Who it goes to

By default, whoever is free in that project. A card is written in a column on your board, not
addressed to anybody, so it arrives as general work.

If you have named a lead in [`agents.task_authors`](configuration.md), the card goes to that role
instead:

```toml
[agents]
task_authors = ["pm"]
```

Now an armed card lands on the lead's plate to be read and broken into real tasks, rather than in
a pool where the nearest idle agent starts work from a title and a due date. That is usually what
you want from a card: the card is the intention, and turning an intention into work someone can
start is a job for your most capable agent.

## The list

`v` swaps the columns for one flat table, sorted by due date, across every column:

```
   ID   COLUMN      DUE        PROJECT     TITLE
   #14  Todo        yesterday  horde       chase the vendor about the encoding  #vendor
 ▸ #12  Todo        in 2d      horde       wire up the importer  #api #p1  ≡  ⚑ armed  💬
   #5   Doing       in 5d      partner-po… fix the auth refresh  ⚑ agents
   #21  Backlog                —           someday: rewrite the parser
```

The board answers *what is where*. The list answers *what is next*, which columns are actively
bad at: work due tomorrow spread across four columns is four places you have to look.

Every row carries everything a card was given — its due date, its project, its tags, and
whether it has a description (`≡`), a thread (`💬`) or an arrangement with the agents
(`⚑`). A card with no project shows `—`, because a personal board has plenty of work that is
not about a repository and a blank cell reads as a field nobody filled in.

Narrow, it drops `PROJECT` and then `COLUMN` rather than shrinking them into stubs. `ID`, `DUE`
and the title always survive.

Everything the board can do to a card, the list can, except arranging: `n` makes one in the
column of the row you are on, `enter` opens it, `X` archives it, `/` filters and `p` scopes.
There is deliberately no dragging — the list is sorted by due date, so a drop would either
reorder nothing or quietly rewrite the date to make itself true. Move work between columns on
the board, which is what a board is for.

### The mouse

| Gesture | Action |
|---|---|
| click a row | put the cursor on it |
| double-click a row | open the card, floating over the list |
| click off a floating card | put it away |
| wheel | move the cursor |

A click on the header, or below the last row, moves nothing. Selecting the last card because
the pointer landed past it is the kind of wrong that looks deliberate.

### A card, floating

From the board, `enter` on a card gives it the whole frame: arranging work is not something
you are doing *to* a session you can still see. From the **list** it floats instead, over the
list it came from:

```
  ID   COLUMN      DUE        PROJECT     TITLE
▸ #12  Todo        in 2d      horde       wire up the importer  #api #p1  ≡  ⚑ armed  💬
  #21  Backlog                —           someday: rewrite the parser
                    ╭ card #12 ────────────────────────────────────────────╮
                    │#12  wire up the importer                            │
                    │Todo  ·  added 2d ago                                │
                    │                                                     │
                    │  due      2026-08-22 · in 2d                        │
                    │  tags     #api #p1                                  │
                    │  project  horde                                     │
                    │  agents   hand over when due within 2d              │
                    │                                                     │
                    │DESCRIPTION                                          │
                    │  Read the CSV in chunks so a 200MB file does not     │
                    │  blow the heap.                                     │
                    │                                                     │
                    │COMMENTS  1                                          │
                    │  josh@joshmacbook   just now                        │
                    │   parked until the schema lands                      │
                    ╰─────────────────────────────────────────────────────╯
```

Same card, same keys, same fields — see [A card](#a-card). The difference is that the list is
still behind it, because reading one row of a list you are scanning is a peek rather than a
departure. `esc` comes back **to the list**, at the row you left; so does clicking anywhere off
the panel, unless you are part-way through typing something, in which case a stray click is not
allowed to throw your words away.

A view you have to re-enter after every card is a view you stop using.

## Columns

Four to start with — Backlog, Todo, Doing, Done — and they are yours to change. `C` adds one,
`R` renames the one you are in, `D` removes it, `<` and `>` move it. Changes are written to
`config.toml`, keeping its comments and key order.

```toml
[kanban]
columns = ["Backlog", "Todo", "Doing", "Done"]
```

A card holds a column *name*, not an index, so editing the list can never silently rewrite
what a card means. Two consequences worth knowing:

- **Removing a column gives its cards to the one before it**, explicitly, and says how many
  moved. The first column has nowhere to send its cards, so it refuses.
- **A card naming a column that is no longer configured is never lost.** It gets a column of
  its own at the end of the board, marked, so you can see it and drag it somewhere that
  exists.

## Where it lives

`~/.config/horde/kanban.jsonl` — its own file, an append-only log the daemon replays on start,
the same shape the task board and the message bus use. The daemon is the only writer.

Nothing else in horde writes to it, and the task board's file is untouched by any of this.

## Configuration

See [configuration](configuration.md#kanban) for the full list. In short:

```toml
[kanban]
columns = ["Backlog", "Todo", "Doing", "Done"]
author = "josh@joshmacbook"   # defaults to $USER@hostname
assist = "2d"                 # the window a newly armed card gets
```
