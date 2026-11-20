# The playground

Play Victris by hand against the bot's own brain, with no clock, and go back to any tick to
try something else. It runs on the Mac and you use it in a browser:

    http://Claras-Mac-mini.local:8790/        (on the Mac itself: http://localhost:8790/)

It is kept running by a launchd agent (`com.victris.playground`). By hand:

    playground/run.sh              # build if needed, then serve on port 8790
    playground/run.sh --build      # build only (after the bot's code changed)
    playground/install.sh          # install or reload the agent; --remove takes it away

## What is real

- Every position comes from the exact engine port (`victris-engine`). Nothing about the
  rules is written again here. A loaded ladder replay is re-simulated and every tick's state
  hash is compared with the one the ladder recorded; the page says how many matched.
- The bot's side is decided by the live bot's own code: the same tracker (it is given only
  the observation and the events the ladder would send that seat) and the same `decide`,
  with the playbook file field-marshal runs live (`data/releases/live/playbooks/flagship.json`).
  "Bot settings" switches to another playbook file, a longer think, or extra settings.
- It is built from the working tree, so it plays the search as the code stands now. The live
  bot is the frozen release; where the two differ, the playbook's switches decide.

## How rewinding works

Positions are kept in a tree. Going back is jumping to a node; playing a different turn
there grows a new branch and the old line stays, listed under "Other moves tried here".
The bot's reply at a position is decided once and kept, so trying another move of yours at
the same tick meets the same reply ("Bot: think again" redoes it). Turns are simultaneous,
as on the ladder: the bot has not seen your turn for the tick when it chooses.

From a replay, "Play <name> from here" takes a seat at any tick; our bot's brain plays the
other seat from then on (on the first tick it can play what the replay recorded).

## What the search panel shows

For the tick just played: the turn the bot chose, the result it expected (hp, from its
side, over the depth it searched), how many iterations it ran, its top candidates with the
share of the search each got, and the turns it expected from you with their shares, your
actual turn marked. This is read from the search itself through a small hook in the policy
(`LAST_ROOT` in `field-marshal/crates/bot/src/policy.rs`); nothing in the bot reads it.

## Files

- `src/main.rs`: the server (plain HTTP on std, one thread owns the game and the brain).
- `web/index.html`: the page. It is read from this folder on every load, so changing it
  needs no build.
- Lines played by hand are written after every tick to `data/playground/games/` as replays
  in the ladder's format, so every replay tool reads them. Playbook files of your own go in
  `data/playground/playbooks/`.

The playground talks to nothing outside this Mac and never touches the bots, the gateway
or their tokens. It is a package of its own, outside field-marshal's workspace, with its
own build folder (`data/build/playground`), so building it never blocks a build there.
