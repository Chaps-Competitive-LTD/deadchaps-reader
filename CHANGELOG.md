# DeadChaps reader

## 1.0.8

- The Light reading speed is now called Slow.

## 1.0.7

- Matches reach DeadChaps sooner: the reader looks for a match's replay details as soon as the game ends it, instead of up to two minutes later.
- A reading speed you can choose in the Companion's Settings: Fast for high-end PCs, Balanced, or Light for low-end PCs.

## 1.0.6

- No match timers in Street Brawl, which has no bridge buffs and no mid boss.

## 1.0.5

- A match read before the game's own match line turns up is no longer taken for the hideout. In Street Brawl this left the reads slow and Discord saying you were in the hideout.

## 1.0.4

- Much faster on PCs short of memory: while a match is on, the reader reads only the part of the game that is in RAM, so the Lobby fills and the timers show far sooner and the game is not slowed by the reader pulling memory back from the disk.

## 1.0.3

- The match timers show even when the reader cannot find the game's own line naming the match, which left some players with no timers at all.

## 1.0.2

- The match timers come back if the reader misses them at the start of a match: it now looks again every five minutes instead of giving up after its first few tries. This happened after watching a replay.

## 1.0.1

- Reads the game far more gently on PCs short of memory. It no longer reads the whole game over and over during a match while looking for the last match's replay details, it waits longer after a slow read, and its reads run at low priority so the game always comes first.

## 1.0.0

- The game reader as its own program, apart from the Companion app. It opens Deadlock with read access only, never writes to it or puts anything into it, and never uses the network.
