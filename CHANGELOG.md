# DeadChaps reader

## 1.0.3

- The match timers show even when the reader cannot find the game's own line naming the match, which left some players with no timers at all.

## 1.0.2

- The match timers come back if the reader misses them at the start of a match: it now looks again every five minutes instead of giving up after its first few tries. This happened after watching a replay.

## 1.0.1

- Reads the game far more gently on PCs short of memory. It no longer reads the whole game over and over during a match while looking for the last match's replay details, it waits longer after a slow read, and its reads run at low priority so the game always comes first.

## 1.0.0

- The game reader as its own program, apart from the Companion app. It opens Deadlock with read access only, never writes to it or puts anything into it, and never uses the network.
