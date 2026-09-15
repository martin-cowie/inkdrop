import './style.css';

interface Printer {
  id: string;
  name: string;
  address: string;
  model: string | null;
  formats: string[];
}

const app = document.querySelector<HTMLDivElement>('#app')!;

function escapeHtml(value: string): string {
  const div = document.createElement('div');
  div.textContent = value;
  return div.innerHTML;
}

function render(printers: Printer[]): void {
  if (printers.length === 0) {
    app.innerHTML = `
      <div class="empty-state">
        <div class="emoji">🤔</div>
        <p>No printers supporting URF or PWG-Raster have been found on the network yet.</p>
      </div>
    `;
    return;
  }

  app.innerHTML = `
    <h1>Drop a PDF on a printer</h1>
    <div class="printer-grid">
      ${printers
        .map(
          (p) => `
        <div class="printer-tile" data-id="${escapeHtml(p.id)}">
          <div class="emoji">🖨️</div>
          <div class="name">${escapeHtml(p.name)}</div>
          <div class="address">${escapeHtml(p.address)}</div>
          <div class="meta">
            ${p.model ? `<span class="model">${escapeHtml(p.model)}</span>` : ''}
            ${p.formats.map((f) => `<span class="badge">${escapeHtml(f)}</span>`).join('')}
          </div>
          <div class="status"></div>
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

function draggedItemIsPdf(dataTransfer: DataTransfer): boolean {
  return Array.from(dataTransfer.items).some(
    (item) => item.kind === 'file' && item.type === 'application/pdf',
  );
}

function wireTile(tile: HTMLDivElement, printer: Printer): void {
  const statusEl = tile.querySelector<HTMLDivElement>('.status')!;
  let resetTimer: ReturnType<typeof setTimeout> | undefined;

  const clearFeedback = () => {
    tile.classList.remove('drag-ok', 'drag-bad');
  };

  tile.addEventListener('dragenter', (event) => {
    event.preventDefault();
  });

  tile.addEventListener('dragover', (event) => {
    event.preventDefault();
    const dataTransfer = event.dataTransfer;
    if (!dataTransfer) return;

    const isPdf = draggedItemIsPdf(dataTransfer);
    dataTransfer.dropEffect = isPdf ? 'copy' : 'none';
    tile.classList.toggle('drag-ok', isPdf);
    tile.classList.toggle('drag-bad', !isPdf);
  });

  tile.addEventListener('dragleave', () => {
    clearFeedback();
  });

  tile.addEventListener('drop', (event) => {
    event.preventDefault();
    clearFeedback();

    const file = event.dataTransfer?.files[0];
    if (!file || file.type !== 'application/pdf') {
      return;
    }

    clearTimeout(resetTimer);
    void sendPrintJob(printer, file, tile, statusEl).then(() => {
      resetTimer = setTimeout(() => {
        tile.classList.remove('success', 'failure', 'sending');
        statusEl.textContent = '';
      }, 3000);
    });
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
