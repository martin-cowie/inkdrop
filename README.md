<p align="center"><img src="assets/logo.png" width="240" alt="inkdrop logo"></p>

# inkdrop

Drag a PDF onto a printer icon and it prints, via IPP. Printers are discovered
automatically on the local network via mDNS, and only printers that handle
**PDF**, **URF** or **PWG-Raster** are shown, with a badge for each. When a
printer doesn't take PDF directly, the PDF is rasterized locally and sent as
PWG-Raster instead.

## Prerequisites

- Rust (stable toolchain, `cargo`)
- Node.js + npm
- The PDFium native library (used to rasterize PDFs — see below)
- At least one printer on the same network/subnet as this machine that
  advertises support for one of those formats (most AirPrint / IPP Everywhere
  printers do). mDNS discovery requires being on the same L2 network segment
  as the printer — it will not find printers across VPNs or routed subnets.

### Getting PDFium

inkdrop renders PDF pages itself (via `pdfium-render`) rather than shelling
out, which means a prebuilt PDFium shared library must be present at
runtime — it is **not** downloaded automatically and is **not** bundled in
this repo (it's a large, platform-specific binary).

1. Download the build for your platform from
   [bblanchon/pdfium-binaries releases](https://github.com/bblanchon/pdfium-binaries/releases)
   — e.g. `pdfium-linux-x64.tgz` for a typical Linux server, or
   `pdfium-mac-arm64.tgz` for Apple Silicon Macs.
2. Extract it and note the path to the `lib/` directory inside (it contains
   `libpdfium.so` / `libpdfium.dylib` / `pdfium.dll`).
3. Point inkdrop at it with an environment variable when running:

   ```sh
   export PDFIUM_DYNAMIC_LIB_PATH=/path/to/extracted/lib
   ```

   If unset, inkdrop looks for it at `native/pdfium/lib` relative to the
   working directory — convenient for local dev if you extract it there.

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
PDFIUM_DYNAMIC_LIB_PATH=/path/to/lib cargo run
```

The server listens on `http://localhost:8080` by default. Open that in a
browser. Set `PORT=<n>` to use a different port.

To list printers that mDNS can't find — such as the [simulated
printer](#simulated-printer) — set `INKDROP_PRINTERS` to a comma-separated list
of their `ipp://` URIs:

```sh
INKDROP_PRINTERS=ipp://localhost:1631/ipp/print PDFIUM_DYNAMIC_LIB_PATH=/path/to/lib cargo run
```

Each is checked over IPP every 10 seconds and shown while it answers and
handles a supported format, so it can be started after inkdrop. Use `cargo run --release`
for an optimized build.

## Testing it

1. Open `http://localhost:8080` in a browser on the same network as a
   qualifying printer.
2. Within a few seconds you should see a 🖨️ tile per printer found, showing
   its name, IPP URL, model (if advertised), and which format(s) it
   supports. If none appear, you'll see a 🤔 with
   an explanation — check the troubleshooting section below.
3. Drag a `.pdf` file from your file manager over a printer tile. The cursor
   should indicate it's droppable, and the tile highlights. Dropping a
   non-PDF file should show the "not allowed" cursor and no highlight.
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
PDFIUM_DYNAMIC_LIB_PATH=/path/to/lib cargo run --bin inkdrop-print -- document.pdf ipp://192.168.1.20:631/ipp/print
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
in the web UI, name it in `INKDROP_PRINTERS` (see below); `inkdrop-print` can
address it directly. It accepts
`application/pdf`, `image/jpeg` and `image/pwg-raster`, so use
`--format pwg-raster` to test conversion. Received jobs are saved in
`.docker/dev/ipp-server/spool/ipp-dev/`. `ippserver` spools whatever it is
sent without checking it, so check a raster job against the PWG spec with
`ippdoclint`:

```sh
docker compose exec ipp-server ippdoclint -v -i image/pwg-raster /spool/ipp-dev/<job-file>.ras
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
- **Printer found but printing fails**: The server re-checks the printer's
  actual IPP attributes right before submitting the job — if its advertised
  mDNS capabilities and its real IPP attributes disagree, printing is
  refused rather than sending a doomed job. It also fails clearly if a
  printer only advertises URF (no PWG-Raster encoder is implemented yet —
  only PWG-Raster conversion and direct PDF pass-through are supported).
- **"failed to load the PDFium library" panic on startup**: `PDFIUM_DYNAMIC_LIB_PATH`
  isn't set (or doesn't point at the right directory) and no system-wide
  PDFium install was found either. See "Getting PDFium" above.
- **Frontend changes don't show up**: You must re-run `npm run build` inside
  `frontend/` — the Rust server only serves the built `frontend/dist`
  output, it doesn't rebuild it for you.

## Development

For frontend iteration with hot reload, run the backend and the Vite dev
server side by side:

```sh
PDFIUM_DYNAMIC_LIB_PATH=/path/to/lib cargo run   # terminal 1, backend on :8080
cd frontend && npm run dev                        # terminal 2, Vite dev server on :5173
```

`frontend/vite.config.ts` proxies `/api/*` requests from the Vite dev server
to `localhost:8080`, so open `http://localhost:5173` while developing.
