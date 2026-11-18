# Replay corpus

Ladder replays fetched by the `Replay corpus` workflow on `main` (`fleet/tools/fetch_replays.py`).
`index.jsonl` has one line per replay; the files are under `replays/`.
The fleet pulls this branch, verifies every replay on the exact engine and indexes it.
