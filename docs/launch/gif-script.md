# 30-second demo GIF — recording script

**Status: [MANUAL — founder].** The recording itself is a founder task. This file is the
exact command sequence to record, plus timing, so the GIF is reproducible and every command
shown is one the [README](../../README.md) quickstart actually ships. Nothing staged, nothing
faked — the whole point of EmbedMind's positioning is that the terminal you see is the
terminal a viewer gets.

## The story in one line

*Your agent forgets between sessions — here it remembers a decision, then recalls it by
meaning, from a single local file, in seconds.*

## Before you hit record (setup, off-camera)

Do all of this **before** recording so the GIF starts clean and contains zero waiting:

```bash
# 1. Build/install the exact binary a viewer would get.
cargo install --path crates/embedmind-cli    # or the prebuilt binary from Releases

# 2. Warm the binary once (first ONNX init is slower; we don't film that).
embedmind --version

# 3. Use a throwaway memory file so the demo is deterministic and repeatable.
export EMBEDMIND_DEMO=/tmp/embedmind-demo.mind      # Windows: $env:EMBEDMIND_DEMO="$env:TEMP\embedmind-demo.mind"
rm -f "$EMBEDMIND_DEMO"

# 4. Terminal: large font, ~90x24, clear prompt. Clear the scrollback.
clear
```

Recommended capture: [`vhs`](https://github.com/charmbracelet/vhs) (scriptable, deterministic
`.gif` output) or `asciinema` + `agg`. A `.tape` file makes the GIF regenerable on every
release; a hand-recorded screen capture is fine too.

## The recording (≈30 s, 5 beats)

Each beat is one on-screen command. Type at a human pace; the pauses are where the viewer
reads the output. Times are cumulative targets, not hard cuts.

| Beat | t (s) | Command (typed on camera) | What the viewer sees / reads |
|---|---|---|---|
| 1 | 0–6 | `embedmind remember "We chose tokio for async I/O — see ADR-003" --file "$EMBEDMIND_DEMO"` | Prints a new memory id, e.g. `01J…  (global)`. "It stored something, instantly." |
| 2 | 6–12 | `embedmind remember "Postgres is the primary datastore; Redis only for rate limits" --file "$EMBEDMIND_DEMO"` | A second id. Two unrelated facts are now in one file. |
| 3 | 12–22 | `embedmind recall "what do we use for concurrency?" --file "$EMBEDMIND_DEMO"` | The **tokio** memory comes back first with a score — note the query never says "tokio" or "async". This is the payoff: semantic recall, not grep. |
| 4 | 22–28 | `embedmind stats --file "$EMBEDMIND_DEMO"` | `live memories: 2`, file size, `embedding model: all-MiniLM-L6-v2-int8`. Proof it's one real local file with a built-in model. |
| 5 | 28–30 | (no command — hold on the stats output) | Freeze frame for the loop. |

## Exact copy-paste block (what actually gets typed)

This is the literal sequence, no `--file` flag if you'd rather show the default
`~/.embedmind/memory.mind` (cleaner on screen, but then delete it in setup instead of using
the temp file):

```bash
embedmind remember "We chose tokio for async I/O — see ADR-003"
embedmind remember "Postgres is the primary datastore; Redis only for rate limits"
embedmind recall "what do we use for concurrency?"
embedmind stats
```

> If you use the default file for a cleaner line, run `rm -f ~/.embedmind/memory.mind`
> (Windows: `Remove-Item $env:USERPROFILE\.embedmind\memory.mind`) in the off-camera setup so
> the demo is deterministic.

## Rules (keep it honest)

- **Every command must be real and unedited.** If a beat is slow on your machine, speed up
  the *whole* GIF uniformly in post — never cut out latency to imply speed we don't have. The
  benchmark table already states the real numbers.
- **The recall in beat 3 must genuinely rely on semantics** — the query wording shares no
  keyword with the stored memory. That's the demo. Don't cherry-pick a query that only works
  by lexical overlap.
- Don't show `serve` in the 30 s GIF — the MCP integration is a separate, longer clip. This
  one is the "10-second wow": remember → recall → it just worked.
- No secrets, no real project paths, no personal data on screen.

## After recording

- Save as `docs/launch/demo.gif` (or wherever the README will reference it) and add the
  image to the README top section in a follow-up commit.
- If recorded with a `.tape`/`.cast` script, commit that script next to this file so the GIF
  can be regenerated when the CLI output changes.
