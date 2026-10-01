/**
 * The inkdrop web app: one tile per printer, each a drop target and file
 * picker that uploads a PDF to the server for printing, showing the
 * printer's state and queue and following the job through it.
 * @module
 */

/** A print job, as the server reports it. */
export interface Job {
  /** The printer's `job-id`. */
  id: number;
  /** The IPP `job-state` keyword, e.g. `pending`, or `forgotten`. */
  state: string;
}

/** What a printer is doing, and its queue. */
export interface PrinterStatus {
  /** `idle`, `processing`, `stopped` or `unreachable`. */
  state: string;
  /** IPP `printer-state-reasons`, e.g. `media-empty-error`. */
  reasons: string[];
  /** The printer's own description of its state, if any. */
  message: string | null;
  /** How many jobs the printer has yet to finish, from any source. */
  queued: number;
  /** Unfinished jobs, in the order the printer will process them. */
  jobs: Job[];
  /** Jobs sent through inkdrop that have finished, oldest first. */
  finished: Job[];
}

/** A printer as the server lists it on `/api/printers`. */
export interface Printer {
  /** Key for `/api/print/{id}`. */
  id: string;
  /** Display name. */
  name: string;
  /** The printer's IPP URI. */
  uri: string;
  /** Make and model, if the printer advertises one. */
  model: string | null;
  /** Labels of the formats it accepts, e.g. `PDF`, `URF`, `PWG-Raster`. */
  formats: string[];
  /** The printer's state and queue, once known. */
  status: PrinterStatus | null;
}

/** How a tile's message is styled. */
type Outcome = 'sending' | 'success' | 'waiting' | 'failure' | 'invalid';

/** A job's progress, as shown on its tile. */
export interface JobProgress {
  /** What to show, e.g. "Your job: 2nd of 3". */
  text: string;
  /** How to style it. */
  outcome: Outcome;
  /** Whether the job has finished, one way or another. */
  finished: boolean;
}

/** A printer's state, as shown on its tile. */
export interface StateSummary {
  /** What to show, e.g. "Stopped: out of paper". */
  text: string;
  /** A class for the tile when the printer needs attention. */
  alert: 'stopped' | 'unreachable' | null;
}

const BRAND_HTML = `
  <div class="brand">
    <img class="brand-mark" src="/logo-mark.png" alt="" width="40" height="40" />
    <span class="brand-name"><span class="ink">ink</span><span class="drop">drop</span></span>
  </div>
`;

const TILE_HTML = `
  <div class="emoji">🖨️</div>
  <div class="name"></div>
  <div class="uri"></div>
  <div class="meta"></div>
  <div class="printer-state"></div>
  <div class="queue"></div>
  <div class="status"></div>
  <input type="file" class="file-input" accept="application/pdf" hidden />
`;

const OUTCOMES: Outcome[] = ['sending', 'success', 'waiting', 'failure', 'invalid'];

const STATE_NAMES: Record<string, string> = {
  idle: 'Ready',
  processing: 'Printing',
  stopped: 'Stopped',
  unreachable: 'Not responding',
};

const REASONS: Record<string, string> = {
  'connecting-to-device': 'connecting',
  'cover-open': 'cover open',
  'door-open': 'door open',
  'input-tray-missing': 'paper tray missing',
  'marker-supply-empty': 'out of ink',
  'marker-supply-low': 'ink low',
  'marker-waste-full': 'waste ink full',
  'media-empty': 'out of paper',
  'media-jam': 'paper jam',
  'media-low': 'paper low',
  'media-needed': 'load paper',
  'moving-to-paused': 'pausing',
  offline: 'offline',
  other: 'needs attention',
  'output-area-full': 'output tray full',
  'output-tray-missing': 'output tray missing',
  paused: 'paused',
  shutdown: 'shut down',
  'spool-area-full': 'memory full',
  'toner-empty': 'out of toner',
  'toner-low': 'toner low',
};

/** The tiles shown in each app element, by printer id. */
const tilesByApp = new WeakMap<HTMLElement, Map<string, Tile>>();

interface Tile {
  element: HTMLDivElement;
  update(printer: Printer): void;
}

/**
 * Escape text for use in HTML text and in quoted attribute values.
 * @param value - The text to escape.
 * @returns `value` with `&`, `<`, `>`, `"` and `'` escaped.
 */
export function escapeHtml(value: string): string {
  const div = document.createElement('div');
  div.textContent = value;
  // innerHTML leaves quotes alone, which would let a printer name or id
  // (both from the network) break out of an attribute.
  return div.innerHTML.replaceAll('"', '&quot;').replaceAll("'", '&#39;');
}

/**
 * Describe one of a printer's `printer-state-reasons`.
 * @param reason - The keyword, e.g. `media-empty-error`.
 * @returns A short description, e.g. "out of paper", or null for reasons
 * that are only informational (`-report`) or `none`.
 */
export function describeReason(reason: string): string | null {
  if (reason === 'none' || reason.endsWith('-report')) {
    return null;
  }
  const keyword = reason.replace(/-(error|warning)$/, '');
  return REASONS[keyword] ?? keyword.replaceAll('-', ' ');
}

/**
 * Summarise a printer's state for its tile.
 * @param status - The printer's status, or null if not yet known.
 * @returns The text to show, e.g. "Stopped: out of paper", and whether the
 * printer needs attention. When a stopped printer gives no recognisable
 * reason, its own message is shown instead.
 */
export function describeState(status: PrinterStatus | null): StateSummary {
  if (!status) {
    return { text: '', alert: null };
  }
  const name = STATE_NAMES[status.state] ?? status.state;
  const reasons = status.reasons.map(describeReason).filter((reason) => reason !== null);
  const detail = reasons.length > 0 ? reasons.join(', ') : status.state === 'stopped' ? status.message : null;
  const alert = status.state === 'stopped' || status.state === 'unreachable' ? status.state : null;
  return { text: detail ? `${name}: ${detail}` : name, alert };
}

/**
 * Describe the length of a printer's queue.
 * @param status - The printer's status, or null if not yet known.
 * @returns E.g. "3 jobs queued", or an empty string if nothing is queued.
 */
export function describeQueue(status: PrinterStatus | null): string {
  const queued = status?.queued ?? 0;
  if (queued === 0) {
    return '';
  }
  return `${queued} ${queued === 1 ? 'job' : 'jobs'} queued`;
}

/**
 * An English ordinal.
 * @param n - A positive whole number.
 * @returns E.g. "1st", "2nd", "11th", "23rd".
 */
export function ordinal(n: number): string {
  const lastTwo = n % 100;
  const suffix = lastTwo >= 11 && lastTwo <= 13 ? 'th' : ({ 1: 'st', 2: 'nd', 3: 'rd' }[n % 10] ?? 'th');
  return `${n}${suffix}`;
}

/**
 * Describe where a job has got to.
 * @param status - The printer's status, or null if not yet known.
 * @param jobId - The job to describe.
 * @returns The job's progress, or null if the status doesn't mention it yet.
 */
export function describeJob(status: PrinterStatus | null, jobId: number): JobProgress | null {
  if (!status) {
    return null;
  }
  const position = status.jobs.findIndex((job) => job.id === jobId);
  if (position >= 0) {
    const queued = Math.max(status.queued, status.jobs.length);
    switch (status.jobs[position].state) {
      case 'processing':
        return { text: 'Printing your job', outcome: 'success', finished: false };
      case 'pending-held':
        return { text: 'Your job is on hold', outcome: 'waiting', finished: false };
      case 'processing-stopped':
        return { text: 'Your job has stopped', outcome: 'waiting', finished: false };
      default:
        return { text: `Your job: ${ordinal(position + 1)} of ${queued}`, outcome: 'success', finished: false };
    }
  }
  const finished = status.finished.find((job) => job.id === jobId);
  switch (finished?.state) {
    case undefined:
      return null;
    case 'completed':
      return { text: 'Printed', outcome: 'success', finished: true };
    case 'canceled':
      return { text: 'Your job was cancelled', outcome: 'failure', finished: true };
    case 'aborted':
      return { text: 'Your job failed', outcome: 'failure', finished: true };
    default:
      return { text: 'Finished', outcome: 'success', finished: true };
  }
}

/**
 * Show printers as drop targets, or an explanation if there are none.
 * Existing tiles are updated in place, so a tile's progress survives updates.
 * @param app - The element to render into.
 * @param printers - The printers to show, in order.
 */
export function render(app: HTMLElement, printers: Printer[]): void {
  if (printers.length === 0) {
    tilesByApp.delete(app);
    app.innerHTML = `
      ${BRAND_HTML}
      <div class="empty-state">
        <div class="emoji">🤔</div>
        <p>No printers that handle supported formats have been found on the network yet.</p>
      </div>
    `;
    return;
  }

  let tiles = tilesByApp.get(app);
  let grid = app.querySelector<HTMLDivElement>('.printer-grid');
  if (!tiles || !grid) {
    app.innerHTML = `
      ${BRAND_HTML}
      <h1>Drop a PDF on a printer, or click one to choose a file</h1>
      <div class="printer-grid"></div>
    `;
    grid = app.querySelector<HTMLDivElement>('.printer-grid')!;
    tiles = new Map();
    tilesByApp.set(app, tiles);
  }

  const ids = new Set(printers.map((printer) => printer.id));
  for (const [id, tile] of tiles) {
    if (!ids.has(id)) {
      tile.element.remove();
      tiles.delete(id);
    }
  }
  printers.forEach((printer, index) => {
    const tile = tiles.get(printer.id) ?? createTile(printer);
    tiles.set(printer.id, tile);
    tile.update(printer);
    if (grid.children[index] !== tile.element) {
      grid.insertBefore(tile.element, grid.children[index] ?? null);
    }
  });
}

/**
 * Whether the drag looks droppable, for cursor/highlight feedback only —
 * not the authoritative check, which happens at drop.
 *
 * Safari's `dataTransfer.items` is empty during dragenter/dragover for file
 * drags — not just missing `.type`, the list itself has length 0 — while
 * Chrome/Firefox already populate it with real MIME types at this point.
 * See https://bugs.webkit.org/show_bug.cgi?id=223517. `dataTransfer.types`
 * is the one signal that's reliable everywhere: it contains "Files" for
 * any file drag, Safari included, even though `items` isn't usable yet.
 *
 * @param dataTransfer - The drag's data.
 * @returns False for anything but a file drag, or for files whose known
 * types don't include PDF; true otherwise.
 */
export function draggedItemLooksDroppable(dataTransfer: DataTransfer): boolean {
  if (!Array.from(dataTransfer.types).includes('Files')) {
    return false;
  }

  const fileItems = Array.from(dataTransfer.items).filter((item) => item.kind === 'file');
  const knownTypes = fileItems.map((item) => item.type).filter((type) => type !== '');
  if (knownTypes.length === 0) {
    return true;
  }

  return knownTypes.includes('application/pdf');
}

/**
 * Whether a file is a PDF, judged by its type or else its extension.
 * @param file - The chosen or dropped file.
 * @returns True if it's `application/pdf` or named `*.pdf`.
 */
export function isPdfFile(file: File): boolean {
  return file.type === 'application/pdf' || file.name.toLowerCase().endsWith('.pdf');
}

function createTile(initial: Printer): Tile {
  const element = document.createElement('div');
  element.className = 'printer-tile';
  element.tabIndex = 0;
  element.setAttribute('role', 'button');
  element.innerHTML = TILE_HTML;
  const part = (selector: string) => element.querySelector<HTMLElement>(selector)!;
  const [nameEl, uriEl, metaEl, stateEl, queueEl, statusEl] = ['.name', '.uri', '.meta', '.printer-state', '.queue', '.status'].map(part);
  const fileInput = part('.file-input') as HTMLInputElement;

  let printer = initial;
  let followedJob: number | undefined;
  let resetTimer: ReturnType<typeof setTimeout> | undefined;

  const show = (text: string, outcome: Outcome | null) => {
    element.classList.remove(...OUTCOMES);
    if (outcome) {
      element.classList.add(outcome);
    }
    statusEl.textContent = text;
  };

  const clearAfter = (ms: number) => {
    clearTimeout(resetTimer);
    resetTimer = setTimeout(() => show('', null), ms);
  };

  const followJob = () => {
    if (followedJob === undefined) {
      return;
    }
    const progress = describeJob(printer.status, followedJob);
    if (!progress) {
      return;
    }
    show(progress.text, progress.outcome);
    if (progress.finished) {
      followedJob = undefined;
      clearAfter(5000);
    }
  };

  const printFile = async (file: File) => {
    clearTimeout(resetTimer);
    followedJob = undefined;

    if (!isPdfFile(file)) {
      show('Not a PDF', 'invalid');
      clearAfter(1300);
      return;
    }

    show('Sending…', 'sending');
    const result = await sendPrintJob(printer.id, file);
    if (!result.ok) {
      show(result.message, 'failure');
      clearAfter(3000);
      return;
    }
    show('Sent to printer', 'success');
    if (result.jobId === null) {
      clearAfter(3000);
      return;
    }
    followedJob = result.jobId;
    followJob();
  };

  const clearFeedback = () => {
    element.classList.remove('drag-ok', 'drag-bad');
  };

  // Drag-and-drop isn't always available (touch devices, some accessibility
  // setups), so clicking or pressing Enter/Space opens a file picker instead.
  element.addEventListener('click', () => fileInput.click());
  element.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      fileInput.click();
    }
  });

  fileInput.addEventListener('change', () => {
    const file = fileInput.files?.[0];
    fileInput.value = ''; // allow picking the same file again next time
    if (file) {
      void printFile(file);
    }
  });

  element.addEventListener('dragenter', (event) => {
    event.preventDefault();
  });

  element.addEventListener('dragover', (event) => {
    event.preventDefault();
    const dataTransfer = event.dataTransfer;
    if (!dataTransfer) return;

    const looksDroppable = draggedItemLooksDroppable(dataTransfer);
    dataTransfer.dropEffect = looksDroppable ? 'copy' : 'none';
    element.classList.toggle('drag-ok', looksDroppable);
    element.classList.toggle('drag-bad', !looksDroppable);
  });

  element.addEventListener('dragleave', () => {
    clearFeedback();
  });

  element.addEventListener('drop', (event) => {
    event.preventDefault();
    clearFeedback();

    const file = event.dataTransfer?.files[0];
    if (file) {
      void printFile(file);
    }
  });

  const update = (latest: Printer) => {
    printer = latest;
    element.dataset.id = printer.id;
    element.setAttribute('aria-label', `Print to ${printer.name}`);
    nameEl.textContent = printer.name;
    uriEl.textContent = printer.uri;
    uriEl.title = printer.uri;
    metaEl.innerHTML = [
      printer.model ? `<span class="model">${escapeHtml(printer.model)}</span>` : '',
      ...printer.formats.map((format) => `<span class="badge">${escapeHtml(format)}</span>`),
    ].join('');

    const state = describeState(printer.status);
    stateEl.textContent = state.text;
    element.classList.toggle('stopped', state.alert === 'stopped');
    element.classList.toggle('unreachable', state.alert === 'unreachable');
    queueEl.textContent = describeQueue(printer.status);
    followJob();
  };

  return { element, update };
}

type PrintResult = { ok: true; jobId: number | null } | { ok: false; message: string };

async function sendPrintJob(printerId: string, file: File): Promise<PrintResult> {
  const body = new FormData();
  body.append('file', file, file.name);

  try {
    const response = await fetch(`/api/print/${encodeURIComponent(printerId)}`, { method: 'POST', body });
    if (!response.ok) {
      const message = await response.text().catch(() => '');
      return { ok: false, message: message || 'Printing failed' };
    }
    const reply: unknown = await response.json().catch(() => null);
    const jobId = (reply as { jobId?: unknown } | null)?.jobId;
    return { ok: true, jobId: typeof jobId === 'number' ? jobId : null };
  } catch {
    return { ok: false, message: 'Printing failed' };
  }
}

/**
 * Re-render whenever the server sends an updated printer list. Malformed
 * events are ignored.
 * @param app - The element to render into.
 * @returns The open event stream.
 */
export function connect(app: HTMLElement): EventSource {
  const source = new EventSource('/api/printers');
  source.onmessage = (event) => {
    try {
      const printers = JSON.parse(event.data) as Printer[];
      render(app, printers);
    } catch {}
  };
  return source;
}

/**
 * Show the empty state, then follow the server's printer list.
 * @param app - The element to render into.
 */
export function start(app: HTMLElement): void {
  render(app, []);
  connect(app);
}
