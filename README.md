<p align="center"><img src="assets/logo.png" width="240" alt="inkdrop logo"></p>

# inkdrop

[![Build](https://github.com/martin-cowie/inkdrop/actions/workflows/build.yml/badge.svg?branch=master)](https://github.com/martin-cowie/inkdrop/actions/workflows/build.yml)

Drag a PDF onto a printer icon and it prints, via IPP. Printers are discovered
automatically on the local network via mDNS, and only printers that handle
**PDF**, **URF** or **PWG-Raster** are shown, with a badge for each. When a
printer doesn't take PDF directly, the PDF is rasterized locally and sent as
PWG-Raster instead.

## Prerequisites

- Rust (stable toolchain, `cargo`)
- Node.js 22.12+ and npm
- At least one printer on the same network/subnet as this machine that
  advertises support for one of those formats (most AirPrint / IPP Everywhere
  printers do). mDNS discovery requires being on the same L2 network segment
  as the printer — it will not find printers across VPNs or routed subnets.

## Build

From the project root:

```sh
cd frontend
npm install
npm run build
cd ..
```

This produces `frontend/dist`, which the Rust server serves as static files.
You need to rebuild the frontend (`npm run build`) after any change to
`frontend/src`.

## Run

```sh
cargo run
```

The server listens on `http://localhost:8080` by default. Open that in a
browser. Set `PORT=<n>` to use a different port.

To list printers that mDNS can't find — such as the [simulated
printer](#simulated-printer) — set `INKDROP_PRINTERS` to a comma-separated list
of their `ipp://` URIs:

```sh
INKDROP_PRINTERS=ipp://localhost:1631/ipp/print cargo run
```

Each is checked over IPP every 10 seconds and shown while it answers and
handles a supported format, so it can be started after inkdrop.

For a release build, use `cargo build --release` (or `cargo run --release`).

## Testing it

1. Open `http://localhost:8080` in a browser on the same network as a
   qualifying printer.
2. Within a few seconds you should see a 🖨️ tile per printer found, showing
   its name, IPP URL, model (if advertised), and which format(s) it
   supports. If none appear, you'll see a 🤔 with
   an explanation — check the troubleshooting section below.
3. Drag a `.pdf` file from your file manager over a printer tile. The cursor
   should indicate it's droppable, and the tile highlights. Dragging a
   non-PDF file over it should show the "not allowed" cursor and a red
   highlight. (Safari doesn't reveal a file's type until it's dropped, so
   there every file looks droppable, and a non-PDF is rejected with "Not a
   PDF" on drop.)
4. Drop the PDF on the tile. The tile shows "Printing…", then "Sent to
   printer" on success, or an error message on failure.
5. Check the physical printer for output.

Watch the server's terminal output for logs — it logs each printer as it's
discovered, and which format it's using to print (`RUST_LOG=inkdrop=debug
cargo run` for more detail, including every mDNS service seen and why it was
or wasn't included).

**Be gentle with cheap/consumer printers during testing.** Their embedded
IPP servers are often single-threaded and can become unresponsive (stop
answering pings entirely) if hit with several large print jobs in quick
succession — if that happens, give it a minute, or power-cycle it.

**Don't have a spare printer, or don't want to burn paper and ink on every
test?** See [Testing the print path](#testing-the-print-path) below.

## Testing the print path

`inkdrop-print` is a second binary that runs just the "print this PDF to that
printer" step — the same conversion and IPP submission code the server uses,
without the web UI or mDNS discovery:

```sh
cargo run --bin inkdrop-print -- document.pdf ipp://192.168.1.20:631/ipp/print
```

It asks the printer what it accepts, converts the PDF if needed (e.g. to
PWG-Raster), submits a Print-Job, and reports the job id and state — or the
printer's status code and `status-message` if it rejects the job. Options:

- `--format auto|pdf|pwg-raster` — `auto` (the default) behaves like the
  server, preferring PDF. The others force that format even if the printer
  doesn't advertise it, which is how to exercise raster conversion against a
  printer that also takes PDF.
- `--title <name>` — job title (defaults to the file name).
- `-v` — debug logs: the print plan, each raster page header, and the IPP
  operation and job attributes sent and received. `-vv` also dumps every
  printer attribute.

### Simulated printer

For a printer that doesn't use paper, run
[ippsample](https://github.com/istopwg/ippsample)'s `ippserver` in Docker:

```sh
./scripts/sim-start.sh     # ipp://localhost:1631/ipp/print
./scripts/sim-stop.sh
```

Set `SIM_PORT` to publish it on a different host port. It doesn't advertise
over mDNS (Docker Desktop can't pass multicast through anyway), so to see it
in the web UI, name it in `INKDROP_PRINTERS` (see [Run](#run)); `inkdrop-print` can
address it directly. It accepts
`application/pdf`, `image/jpeg` and `image/pwg-raster`, so use
`--format pwg-raster` to test conversion. Received jobs are saved in
`.docker/dev/ipp-server/spool/ipp-sim/`. `ippserver` spools whatever it is
sent without checking it, so check a raster job against the PWG spec with
`ippdoclint`:

```sh
docker compose exec ipp-server ippdoclint -v -i image/pwg-raster /spool/ipp-sim/<job-file>.ras
```

Server logs: `docker compose logs -f ipp-server`.

## Troubleshooting

- **No printers show up**: Confirm the printer and this machine are on the
  same subnet, and no firewall is blocking mDNS (UDP 5353) or the printer's
  IPP port (usually 631). On macOS, System Settings → Firewall may need an
  exception for the `inkdrop` binary the first time you run it. Also check
  with `RUST_LOG=inkdrop=debug` — it logs the raw `pdl` TXT value for every
  mDNS service it sees, which tells you exactly what formats the printer is
  (or isn't) advertising.
- **Printer found but printing fails**: Right before submitting the job, the
  server asks the printer over IPP which formats it accepts, whatever mDNS
  advertised. It sends PDF if the printer takes it, otherwise PWG-Raster; if
  the printer takes neither, printing is refused rather than sending a
  doomed job, and the tile shows "printer no longer supports PDF". This
  includes printers that only take URF: they are listed, but no URF encoder
  is implemented yet.
- **"PDFium is unavailable" on startup**: the library isn't in the directory
  `cargo build` downloaded it to (or `PDFIUM_DYNAMIC_LIB_PATH` doesn't point
  at the right directory) and no system-wide PDFium install was found either.
  See "Getting PDFium" above.
- **Frontend changes don't show up**: You must re-run `npm run build` inside
  `frontend/` — the Rust server only serves the built `frontend/dist`
  output, it doesn't rebuild it for you.

## Development

For frontend iteration with hot reload, run the backend and the Vite dev
server side by side:

```sh
cargo run                                         # terminal 1, backend on :8080
cd frontend && npm run dev                        # terminal 2, Vite dev server on :5173
```

`frontend/vite.config.ts` proxies `/api/*` requests from the Vite dev server
to `localhost:8080`, so open `http://localhost:5173` while developing.

## Automated tests

```sh
cargo test                     # Rust unit and integration tests
cd frontend && npm test        # frontend unit and integration tests (Vitest)
```

No printer, Docker or network setup is needed. The Rust tests run against a
fake IPP printer (`tests/support/mod.rs`) and generate their PDFs on the fly;
`cargo build` downloads PDFium for them, as for inkdrop itself.

- **Unit tests** sit beside the code: `#[cfg(test)]` modules in `src/*.rs`,
  and `frontend/src/*.test.ts`.
- **Integration tests** are in `tests/` and `frontend/tests/integration/`.
  `tests/server.rs` and `tests/print_cli.rs` run the real `inkdrop` and
  `inkdrop-print` binaries against fake printers. `tests/mdns_discovery.rs`
  advertises fake printers over real mDNS, so it needs a network interface
  with multicast (loopback alone won't do). The frontend integration test
  loads `index.html` and `main.ts` against a fake backend.

To measure coverage (CI fails below 95%):

```sh
cargo llvm-cov                 # needs cargo-llvm-cov; add --html for a report
cd frontend && npm run coverage
```
