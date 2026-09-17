# printer-sim

A fake IPP printer for testing inkdrop without burning real paper and ink.

It advertises itself over mDNS exactly like a real printer would (so
inkdrop's discovery picks it up), answers `Get-Printer-Attributes` with a
believable capability set, and accepts `Print-Job` requests — saving
whatever it receives to `jobs/` instead of printing it.

If the job's `document-format` is `image/pwg-raster`, it also **decodes the
raster back into PNGs**, one per page. That's the actual point: it lets you
visually confirm a printed page came out correctly — right content, right
orientation, right size — without ever touching hardware. If the raster
fails to decode, it responds with `client-error-document-format-error`, the
same way a real printer would react to a malformed stream — so a broken
encoder in the main app shows up immediately as a rejected job, not as
garbled paper.

This is a fully independent crate (its own `Cargo.toml`), not a workspace
member of the main `inkdrop` binary — it exists purely as a development
tool alongside it.

## Run

```sh
cd simulator
cargo run
```

By default it advertises `Inkdrop Simulated Printer` over mDNS and listens
on an OS-assigned ephemeral port — check the startup log for which one, or
just let mDNS discovery find it, which is how inkdrop finds it too. This
also means a leftover instance from a previous run can never block a new
one with an "address already in use" error.

For a fixed, predictable port (handy for manual `curl`/`ipptool` testing
without checking the log each time), set `PORT`:

```sh
PORT=9631 JOBS_DIR=jobs cargo run
```

(9631, not 631 — that's a privileged port most systems won't let a normal
process bind to.)

Then point inkdrop at your own machine — run inkdrop as usual (it discovers
the simulator the same way it discovers a real printer) and drop a PDF onto
the "Inkdrop Simulated Printer" tile.

## Inspecting a job

Every job lands in `jobs/` (created on first run, gitignored):

- `NNNN-<name>.pwg` / `.pdf` / `.urf` / `.bin` — the raw bytes inkdrop sent,
  named by job id and job title, extension by declared document-format.
- `NNNN-<name>-pageN.png` — for PWG-Raster jobs, one PNG per decoded page.

Check the simulator's own log output for a one-line summary of each job
(document format, byte count, page count, and the output path).

## What it doesn't do

- No TLS / `_ipps._tcp` — only plain `_ipp._tcp`. inkdrop prefers IPPS when
  both are available, so this simulator only ever advertises the transport
  it actually implements.
- No URF decoding (URF jobs are saved raw, but not previewed) — inkdrop
  doesn't have a URF encoder yet either, so this isn't exercised.
- No real job queue, `Get-Jobs`, or `Cancel-Job` — jobs "complete" the
  instant they're received. Anything beyond `Get-Printer-Attributes` and
  `Print-Job` gets a generic `successful-ok` so clients don't hang, but
  isn't meaningfully implemented.
