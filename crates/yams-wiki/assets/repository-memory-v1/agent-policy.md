## Project memory

- Search early with `yams --json -k 5 "<focused question>"` when project history, conventions, decisions, or prior failures may matter.
- Treat hits as leads and verify consequential claims against current code, tests, documentation, or Git history.
- Interpret exit status 1 as an empty result from a searched store. Retry exit status 3, below the confidence gate, once with `--no-gate` for possible leads. Treat exit status 4 as an operational failure whose JSON names the fault — when the JSON `code` is `store_missing`, run `yams --index` from the project — and never as missing memory.
- Never initialize a missing corpus unless the user explicitly asks.
- Before changing `.agents/memory/`, inspect `git status --short .agents/memory/` and do not stack changes on another writer's uncommitted work.
- Before any memory write, read `.agents/memory/SCHEMA.md` completely and follow it. `yams-wiki write` regenerates the catalog automatically; after direct page edits or recovery, run `yams-wiki catalog .agents/memory`. Never hand-edit the generated index region.
- Preserve only verified, durable, reusable knowledge; never preserve secrets, transcripts, speculation, or temporary task progress.
