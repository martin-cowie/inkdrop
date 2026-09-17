import './style.css';

interface Printer {
  id: string;
  name: string;
  address: string;
  model: string | null;
  formats: string[];
}

const app = document.querySelector<HTMLDivElement>('#app')!;

const BRAND_HTML = `
  <div class="brand">
    <img class="brand-mark" src="/logo-mark.png" alt="" width="40" height="40" />
    <span class="brand-name"><span class="ink">ink</span><span class="drop">drop</span></span>
  </div>
`;

function escapeHtml(value: string): string {
  const div = document.createElement('div');
  div.textContent = value;
  return div.innerHTML;
}

function render(printers: Printer[]): void {
  if (printers.length === 0) {
    app.innerHTML = `
      ${BRAND_HTML}
      <div class="empty-state">
        <div class="emoji">🤔</div>
        <p>No printers supporting URF or PWG-Raster have been found on the network yet.</p>
      </div>
    `;
    return;
  }

  app.innerHTML = `
    ${BRAND_HTML}
    <h1>Drop a PDF on a printer, or click one to choose a file</h1>
    <div class="printer-grid">
      ${printers
        .map(
          (p) => `
        <div class="printer-tile" data-id="${escapeHtml(p.id)}" tabindex="0" role="button"
             aria-label="Print to ${escapeHtml(p.name)}">
          <div class="emoji">🖨️</div>
          <div class="name">${escapeHtml(p.name)}</div>
          <div class="address">${escapeHtml(p.address)}</div>
          <div class="meta">
            ${p.model ? `<span class="model">${escapeHtml(p.model)}</span>` : ''}
            ${p.formats.map((f) => `<span class="badge">${escapeHtml(f)}</span>`).join('')}
          </div>
          <div class="status"></div>
          <input type="file" class="file-input" accept="application/pdf" hidden />
        </div>
      `,
        )
        .join('')}
    </div>
  `;

  for (const printer of printers) {
    const tile = app.querySelector<HTMLDivElement>(`.printer-tile[data-id="${cssEscape(printer.id)}"]`);
    if (tile) {
      wireTile(tile, printer);
    }
  }
}

function cssEscape(value: string): string {
  return typeof CSS !== 'undefined' && CSS.escape ? CSS.escape(value) : value;
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
 */
function draggedItemLooksDroppable(dataTransfer: DataTransfer): boolean {
  if (!Array.from(dataTransfer.types).includes('Files')) {
    return false; // not a file drag at all (e.g. dragging selected text)
  }

  const fileItems = Array.from(dataTransfer.items).filter((item) => item.kind === 'file');
  const knownTypes = fileItems.map((item) => item.type).filter((type) => type !== '');
  if (knownTypes.length === 0) {
    return true; // Safari: items unavailable this early — stay permissive
  }

  return knownTypes.includes('application/pdf');
}

function isPdfFile(file: File): boolean {
  return file.type === 'application/pdf' || file.name.toLowerCase().endsWith('.pdf');
}

function wireTile(tile: HTMLDivElement, printer: Printer): void {
  const statusEl = tile.querySelector<HTMLDivElement>('.status')!;
  const fileInput = tile.querySelector<HTMLInputElement>('.file-input')!;
  let resetTimer: ReturnType<typeof setTimeout> | undefined;

  const clearFeedback = () => {
    tile.classList.remove('drag-ok', 'drag-bad');
  };

  const printFile = (file: File) => {
    clearTimeout(resetTimer);

    if (!isPdfFile(file)) {
      tile.classList.remove('success', 'failure');
      tile.classList.add('invalid');
      statusEl.textContent = 'Not a PDF';
      resetTimer = setTimeout(() => {
        tile.classList.remove('invalid');
        statusEl.textContent = '';
      }, 1300);
      return;
    }

    void sendPrintJob(printer, file, tile, statusEl).then(() => {
      resetTimer = setTimeout(() => {
        tile.classList.remove('success', 'failure', 'sending');
        statusEl.textContent = '';
      }, 3000);
    });
  };

  // Drag-and-drop isn't always available (touch devices, some accessibility
  // setups) — clicking or pressing Enter/Space opens a native file picker
  // as a fallback. The hidden <input> does the actual browsing.
  tile.addEventListener('click', () => fileInput.click());
  tile.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      fileInput.click();
    }
  });

  fileInput.addEventListener('change', () => {
    const file = fileInput.files?.[0];
    fileInput.value = ''; // allow picking the same file again next time
    if (file) {
      printFile(file);
    }
  });

  tile.addEventListener('dragenter', (event) => {
    event.preventDefault();
  });

  tile.addEventListener('dragover', (event) => {
    event.preventDefault();
    const dataTransfer = event.dataTransfer;
    if (!dataTransfer) return;

    const looksDroppable = draggedItemLooksDroppable(dataTransfer);
    dataTransfer.dropEffect = looksDroppable ? 'copy' : 'none';
    tile.classList.toggle('drag-ok', looksDroppable);
    tile.classList.toggle('drag-bad', !looksDroppable);
  });

  tile.addEventListener('dragleave', () => {
    clearFeedback();
  });

  tile.addEventListener('drop', (event) => {
    event.preventDefault();
    clearFeedback();

    const file = event.dataTransfer?.files[0];
    if (file) {
      printFile(file);
    }
  });
}

async function sendPrintJob(
  printer: Printer,
  file: File,
  tile: HTMLDivElement,
  statusEl: HTMLDivElement,
): Promise<void> {
  tile.classList.remove('success', 'failure');
  tile.classList.add('sending');
  statusEl.textContent = 'Printing…';

  const body = new FormData();
  body.append('file', file, file.name);

  try {
    const response = await fetch(`/api/print/${encodeURIComponent(printer.id)}`, {
      method: 'POST',
      body,
    });

    tile.classList.remove('sending');

    if (response.ok) {
      tile.classList.add('success');
      statusEl.textContent = 'Sent to printer';
    } else {
      const message = await response.text().catch(() => '');
      tile.classList.add('failure');
      statusEl.textContent = message || 'Printing failed';
    }
  } catch {
    tile.classList.remove('sending');
    tile.classList.add('failure');
    statusEl.textContent = 'Printing failed';
  }
}

function connect(): void {
  const source = new EventSource('/api/printers');
  source.onmessage = (event) => {
    try {
      const printers = JSON.parse(event.data) as Printer[];
      render(printers);
    } catch {
      // ignore malformed events
    }
  };
}

render([]);
connect();
