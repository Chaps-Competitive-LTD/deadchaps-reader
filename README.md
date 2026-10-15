# DeadChaps reader

The game reader of the [DeadChaps Companion](https://deadchaps.gg/companion). It is the only part of the Companion that touches Deadlock, and it is published here so anyone can see exactly what it does.

It reads. It never writes to the game, never injects anything into it, and never uses the network.

## What it reads

The reader opens the running Deadlock process with read access only and looks through its memory for:

- **Your match history**, as the game holds it: match id, hero, result, K/D/A, souls and so on, so a match shows in the Companion the moment it ends.
- **The players in your match**: Steam id, name, team, hero, level, souls, K/D/A and rank badge, for live team scouting.
- **The match's rules object**: the clock, the game state, the winner and the draft's bans, for the objective timers.
- **The game's console lines** that say a match started or ended.
- **Steam rich presence** the game holds: whether you are queueing, and what your Steam friends are doing in Deadlock.
- **Replay addresses** (salts) for matches you played, which the Companion passes on so the match can be fetched and shown quickly.

Everything it finds goes to the Companion, the program that started it, and nowhere else.

## What it never does

- It never writes to the game's memory.
- It never injects code, loads anything into the game, hooks the game's functions or starts threads in it.
- It never uses the network. The one question it needs answered from outside (whether Valve's replay server has a file) it asks the Companion, which asks Valve.
- It reads no files of yours. The one file it writes is a short list of matches it saw end, in the Companion's own data folder.

## How the Companion uses it

The Companion downloads this program from deadchaps.gg, checks its signature against the same key that signs the Companion's own updates, and starts it. They talk over the reader's standard input and output, one JSON object a line ([`core/src/protocol.rs`](core/src/protocol.rs)). The reader exits when the Companion closes.

Turning off **Read match history from the game** in the Companion's settings stops the reader opening the game at all.

## Check it yourself

**Windows**

- In [Process Explorer](https://learn.microsoft.com/sysinternals/downloads/process-explorer), use Find, Find Handle or DLL, and search for `deadlock.exe`. The only DeadChaps program that keeps a handle to the game open is `deadchaps-reader.exe`, and its access is `0x1010`: `PROCESS_VM_READ` and `PROCESS_QUERY_LIMITED_INFORMATION`. Neither can change the game. (The Companion itself only ever opens the game for a moment with `PROCESS_QUERY_LIMITED_INFORMATION`, to check that a window belongs to Deadlock, and closes it straight away.)
- List the reader's imports with `dumpbin /imports deadchaps-reader.exe`. It uses `OpenProcess`, `VirtualQueryEx` and `ReadProcessMemory`. It has no `WriteProcessMemory`, `CreateRemoteThread`, `VirtualAllocEx` or `SetWindowsHookEx`.
- The Companion app itself has no `ReadProcessMemory` at all.

**Linux** (Deadlock under Proton)

- The reader opens `/proc/<pid>/mem` read-only.
- Most Linux systems only let a program read another program's memory if it started it, and Steam starts Deadlock. So a small helper, [`memgrant`](memgrant/src/main.rs), is given that one right once, with your password. It opens the game's memory read-only, hands the open file to the reader, and exits. It refuses any process that is not Deadlock or that belongs to another user.

**Build it**

With [Rust](https://rustup.rs) installed:

```
cargo build --release -p deadchaps-reader
```

The program is `target/release/deadchaps-reader`. Run `deadchaps-reader version` to see its version and protocol.

## What is where

- [`core/`](core): the reader itself. The memory access is in [`process.rs`](core/src/process.rs) (Windows) and [`process_linux.rs`](core/src/process_linux.rs) (Linux), behind the `process` feature. Everything else works on bytes already read.
- [`cli/`](cli): the `deadchaps-reader` program the Companion starts.
- [`memgrant/`](memgrant): the Linux helper.

## Licence

[PolyForm Noncommercial 1.0.0](LICENSE.md). You are free to read, build and run it for any non-commercial purpose. Commercial use needs our permission.

Questions: [deadchaps.gg](https://deadchaps.gg) or the DeadChaps Discord.
