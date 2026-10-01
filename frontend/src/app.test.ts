import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  FakeEventSource,
  chooseFiles,
  dataTransfer,
  dragEvent,
  pdfFile,
  printer,
  settle,
  status,
} from '../tests/helpers';
import {
  connect,
  describeJob,
  describeQueue,
  describeReason,
  describeState,
  draggedItemLooksDroppable,
  escapeHtml,
  isPdfFile,
  ordinal,
  render,
  start,
} from './app';

let app: HTMLDivElement;

beforeEach(() => {
  app = document.createElement('div');
  document.body.replaceChildren(app);
});

function tileParts(root: HTMLElement = app) {
  const tile = root.querySelector<HTMLDivElement>('.printer-tile')!;
  return {
    tile,
    status: tile.querySelector<HTMLDivElement>('.status')!,
    input: tile.querySelector<HTMLInputElement>('.file-input')!,
  };
}

function mockFetch(response: () => Promise<Response>) {
  const fetch = vi.fn<typeof globalThis.fetch>(response);
  vi.stubGlobal('fetch', fetch);
  return fetch;
}

describe('escapeHtml', () => {
  it('escapes markup and quotes', () => {
    expect(escapeHtml(`<b>"Tom" & 'Jerry'</b>`)).toBe('&lt;b&gt;&quot;Tom&quot; &amp; &#39;Jerry&#39;&lt;/b&gt;');
  });

  it('leaves plain text alone', () => {
    expect(escapeHtml('Office Printer 2')).toBe('Office Printer 2');
  });
});

describe('describeReason', () => {
  it('describes warnings and errors, and hides reports', () => {
    expect(describeReason('media-empty-error')).toBe('out of paper');
    expect(describeReason('toner-low-warning')).toBe('toner low');
    expect(describeReason('paused')).toBe('paused');
    expect(describeReason('media-empty-report')).toBeNull();
    expect(describeReason('none')).toBeNull();
  });

  it('falls back to the keyword itself', () => {
    expect(describeReason('interpreter-resource-unavailable-error')).toBe('interpreter resource unavailable');
  });
});

describe('describeState', () => {
  it('says nothing until the state is known', () => {
    expect(describeState(null)).toEqual({ text: '', alert: null });
  });

  it('names the state, with any reasons', () => {
    expect(describeState(status())).toEqual({ text: 'Ready', alert: null });
    expect(describeState(status({ state: 'processing', reasons: ['toner-low-warning', 'other-report'] }))).toEqual({
      text: 'Printing: toner low',
      alert: null,
    });
    expect(describeState(status({ state: 'stopped', reasons: ['media-jam-error', 'cover-open-error'] }))).toEqual({
      text: 'Stopped: paper jam, cover open',
      alert: 'stopped',
    });
    expect(describeState(status({ state: 'unreachable' }))).toEqual({ text: 'Not responding', alert: 'unreachable' });
    expect(describeState(status({ state: 'warming-up' })).text).toBe('warming-up');
  });

  it("falls back to a stopped printer's own message", () => {
    expect(describeState(status({ state: 'stopped', message: 'Call for service' })).text).toBe('Stopped: Call for service');
    expect(describeState(status({ state: 'stopped' })).text).toBe('Stopped');
    expect(describeState(status({ message: 'Sleeping' })).text).toBe('Ready');
  });
});

describe('describeQueue', () => {
  it('counts queued jobs', () => {
    expect(describeQueue(null)).toBe('');
    expect(describeQueue(status())).toBe('');
    expect(describeQueue(status({ queued: 1 }))).toBe('1 job queued');
    expect(describeQueue(status({ queued: 4 }))).toBe('4 jobs queued');
  });
});

describe('ordinal', () => {
  it('uses the right suffix', () => {
    expect([1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 101, 111].map(ordinal)).toEqual([
      '1st', '2nd', '3rd', '4th', '11th', '12th', '13th', '21st', '22nd', '23rd', '101st', '111th',
    ]);
  });
});

describe('describeJob', () => {
  const queue = (state: string) => status({ queued: 3, jobs: [{ id: 1, state: 'processing' }, { id: 7, state }] });

  it("gives a queued job's position", () => {
    expect(describeJob(queue('pending'), 7)).toEqual({ text: 'Your job: 2nd of 3', outcome: 'success', finished: false });
    expect(describeJob(status({ queued: 0, jobs: [{ id: 7, state: 'pending' }] }), 7)?.text).toBe('Your job: 1st of 1');
  });

  it('describes jobs printing, held or stopped', () => {
    expect(describeJob(queue('processing'), 7)?.text).toBe('Printing your job');
    expect(describeJob(queue('pending-held'), 7)).toEqual({ text: 'Your job is on hold', outcome: 'waiting', finished: false });
    expect(describeJob(queue('processing-stopped'), 7)?.text).toBe('Your job has stopped');
  });

  it('describes finished jobs', () => {
    const finished = (state: string) => describeJob(status({ finished: [{ id: 7, state }] }), 7);
    expect(finished('completed')).toEqual({ text: 'Printed', outcome: 'success', finished: true });
    expect(finished('canceled')).toEqual({ text: 'Your job was cancelled', outcome: 'failure', finished: true });
    expect(finished('aborted')).toEqual({ text: 'Your job failed', outcome: 'failure', finished: true });
    expect(finished('forgotten')).toEqual({ text: 'Finished', outcome: 'success', finished: true });
  });

  it('knows nothing of jobs the status does not mention', () => {
    expect(describeJob(null, 7)).toBeNull();
    expect(describeJob(queue('pending'), 99)).toBeNull();
  });
});

describe('draggedItemLooksDroppable', () => {
  it('rejects drags that are not files', () => {
    expect(draggedItemLooksDroppable(dataTransfer({ types: ['text/plain'] }))).toBe(false);
  });

  it('accepts file drags whose types are not known yet, as in Safari', () => {
    expect(draggedItemLooksDroppable(dataTransfer({ items: [] }))).toBe(true);
    expect(draggedItemLooksDroppable(dataTransfer({ items: [{ kind: 'file', type: '' }] }))).toBe(true);
  });

  it('accepts PDFs and rejects other files by MIME type', () => {
    const pdf = { kind: 'file', type: 'application/pdf' };
    const png = { kind: 'file', type: 'image/png' };
    const text = { kind: 'string', type: 'application/pdf' };
    expect(draggedItemLooksDroppable(dataTransfer({ items: [png, pdf] }))).toBe(true);
    expect(draggedItemLooksDroppable(dataTransfer({ items: [png] }))).toBe(false);
    expect(draggedItemLooksDroppable(dataTransfer({ items: [text, png] }))).toBe(false);
  });
});

describe('isPdfFile', () => {
  it('goes by MIME type or, failing that, extension', () => {
    expect(isPdfFile(pdfFile('scan', 'application/pdf'))).toBe(true);
    expect(isPdfFile(pdfFile('SCAN.PDF', ''))).toBe(true);
    expect(isPdfFile(pdfFile('notes.txt', 'text/plain'))).toBe(false);
  });
});

describe('render', () => {
  it('shows an empty state when there are no printers', () => {
    render(app, []);
    expect(app.querySelector('.brand')).not.toBeNull();
    expect(app.querySelector('.empty-state')?.textContent).toContain('No printers');
    expect(app.querySelector('.printer-tile')).toBeNull();
  });

  it('shows a tile per printer with its details', () => {
    render(app, [printer(), printer({ id: 'p2', name: 'Attic', model: null, formats: ['PWG-Raster'] })]);

    const tiles = app.querySelectorAll<HTMLDivElement>('.printer-tile');
    expect(tiles).toHaveLength(2);
    expect(app.querySelector('h1')?.textContent).toContain('Drop a PDF');

    const [office, attic] = tiles;
    expect(office.dataset.id).toBe('p1');
    expect(office.getAttribute('aria-label')).toBe('Print to Office');
    expect(office.querySelector('.name')?.textContent).toBe('Office');
    expect(office.querySelector('.uri')?.textContent).toBe('ipp://10.0.0.5:631/ipp/print');
    expect(office.querySelector('.uri')?.getAttribute('title')).toBe('ipp://10.0.0.5:631/ipp/print');
    expect(office.querySelector('.model')?.textContent).toBe('LaserJet 9000');
    expect([...office.querySelectorAll('.badge')].map((b) => b.textContent)).toEqual(['PDF', 'URF']);

    expect(attic.querySelector('.model')).toBeNull();
    expect([...attic.querySelectorAll('.badge')].map((b) => b.textContent)).toEqual(['PWG-Raster']);
  });

  it('treats printer details from the network as text, not markup', () => {
    vi.stubGlobal('CSS', { escape: (value: string) => value.replace(/["\\]/g, '\\$&') });
    const hostile = `x" onmouseover="alert(1)`;
    render(app, [printer({ id: hostile, name: `<img src=x onerror=alert(1)> ${hostile}`, model: '<b>bold</b>' })]);

    const { tile } = tileParts();
    expect(tile.dataset.id).toBe(hostile);
    expect(tile.getAttribute('onmouseover')).toBeNull();
    expect(tile.getAttribute('aria-label')).toBe(`Print to <img src=x onerror=alert(1)> ${hostile}`);
    expect(tile.querySelector('img')).toBeNull();
    expect(tile.querySelector('.model')?.textContent).toBe('<b>bold</b>');
  });

  it('replaces the previous printers', () => {
    render(app, [printer()]);
    render(app, [printer({ id: 'p9', name: 'New' })]);
    expect([...app.querySelectorAll('.printer-tile')].map((t) => (t as HTMLElement).dataset.id)).toEqual(['p9']);
  });

  it('updates tiles in place, in the order given', () => {
    render(app, [printer({ id: 'a', name: 'A' }), printer({ id: 'b', name: 'B' })]);
    const [a, b] = app.querySelectorAll<HTMLDivElement>('.printer-tile');
    a.querySelector('.status')!.textContent = 'in progress';

    render(app, [printer({ id: 'b', name: 'B' }), printer({ id: 'a', name: 'A2', model: null, formats: [] })]);

    const tiles = [...app.querySelectorAll<HTMLDivElement>('.printer-tile')];
    expect(tiles).toEqual([b, a]);
    expect(a.querySelector('.name')?.textContent).toBe('A2');
    expect(a.getAttribute('aria-label')).toBe('Print to A2');
    expect(a.querySelector('.meta')?.innerHTML).toBe('');
    expect(a.querySelector('.status')?.textContent).toBe('in progress');
  });

  it("shows each printer's state and queue", () => {
    render(app, [printer({ status: status({ state: 'processing', queued: 2 }) })]);
    const { tile } = tileParts();
    expect(tile.querySelector('.printer-state')?.textContent).toBe('Printing');
    expect(tile.querySelector('.queue')?.textContent).toBe('2 jobs queued');
    expect(tile.className).toBe('printer-tile');

    render(app, [printer({ status: status({ state: 'stopped', reasons: ['media-empty-error'] }) })]);
    expect(tile.querySelector('.printer-state')?.textContent).toBe('Stopped: out of paper');
    expect(tile.querySelector('.queue')?.textContent).toBe('');
    expect(tile.classList.contains('stopped')).toBe(true);

    render(app, [printer({ status: status({ state: 'unreachable' }) })]);
    expect(tile.classList.contains('stopped')).toBe(false);
    expect(tile.classList.contains('unreachable')).toBe(true);
  });
});

describe('a printer tile', () => {
  beforeEach(() => {
    render(app, [printer({ id: 'office/1' })]);
  });

  describe('opening the file picker', () => {
    it('opens on click', () => {
      const { tile, input } = tileParts();
      const click = vi.spyOn(input, 'click').mockImplementation(() => {});
      tile.click();
      expect(click).toHaveBeenCalledTimes(1);
    });

    it('opens on Enter or Space, and ignores other keys', () => {
      const { tile, input } = tileParts();
      const click = vi.spyOn(input, 'click').mockImplementation(() => {});

      for (const key of ['Enter', ' ']) {
        const event = new KeyboardEvent('keydown', { key, cancelable: true });
        tile.dispatchEvent(event);
        expect(event.defaultPrevented).toBe(true);
      }
      const other = new KeyboardEvent('keydown', { key: 'a', cancelable: true });
      tile.dispatchEvent(other);

      expect(click).toHaveBeenCalledTimes(2);
      expect(other.defaultPrevented).toBe(false);
    });

    it('prints the chosen file and resets the input', async () => {
      const fetch = mockFetch(async () => new Response(null, { status: 204 }));
      const { input } = tileParts();

      chooseFiles(input, [pdfFile()]);
      await settle();

      expect(fetch).toHaveBeenCalledTimes(1);
      expect(input.value).toBe('');
    });

    it('does nothing when the picker is cancelled', () => {
      const fetch = mockFetch(async () => new Response(null, { status: 204 }));
      chooseFiles(tileParts().input, []);
      expect(fetch).not.toHaveBeenCalled();
    });
  });

  describe('drag and drop', () => {
    it('accepts drag entry', () => {
      const event = dragEvent('dragenter', dataTransfer());
      tileParts().tile.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(true);
    });

    it('highlights a PDF being dragged over it', () => {
      const { tile } = tileParts();
      const transfer = dataTransfer({ items: [{ kind: 'file', type: 'application/pdf' }] });

      tile.dispatchEvent(dragEvent('dragover', transfer));

      expect(transfer.dropEffect).toBe('copy');
      expect(tile.classList.contains('drag-ok')).toBe(true);
      expect(tile.classList.contains('drag-bad')).toBe(false);
    });

    it('warns about anything else being dragged over it', () => {
      const { tile } = tileParts();
      const transfer = dataTransfer({ items: [{ kind: 'file', type: 'image/png' }] });

      tile.dispatchEvent(dragEvent('dragover', transfer));

      expect(transfer.dropEffect).toBe('none');
      expect(tile.classList.contains('drag-bad')).toBe(true);
      expect(tile.classList.contains('drag-ok')).toBe(false);
    });

    it('ignores dragover without data', () => {
      const { tile } = tileParts();
      const event = dragEvent('dragover');
      tile.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(true);
      expect(tile.className).toBe('printer-tile');
    });

    it('clears the highlight when the drag leaves', () => {
      const { tile } = tileParts();
      tile.dispatchEvent(dragEvent('dragover', dataTransfer()));
      tile.dispatchEvent(dragEvent('dragleave'));
      expect(tile.className).toBe('printer-tile');
    });

    it('prints a dropped file', async () => {
      const fetch = mockFetch(async () => new Response(null, { status: 204 }));
      const { tile } = tileParts();
      tile.dispatchEvent(dragEvent('dragover', dataTransfer()));

      const drop = dragEvent('drop', dataTransfer({ files: [pdfFile()] }));
      tile.dispatchEvent(drop);
      await settle();

      expect(drop.defaultPrevented).toBe(true);
      expect(tile.classList.contains('drag-ok')).toBe(false);
      expect(fetch).toHaveBeenCalledTimes(1);
    });

    it('ignores drops without a file', () => {
      const fetch = mockFetch(async () => new Response(null, { status: 204 }));
      const { tile } = tileParts();
      tile.dispatchEvent(dragEvent('drop'));
      tile.dispatchEvent(dragEvent('drop', dataTransfer({ files: [] })));
      expect(fetch).not.toHaveBeenCalled();
    });
  });

  describe('printing', () => {
    beforeEach(() => {
      vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    function drop(file: File) {
      tileParts().tile.dispatchEvent(dragEvent('drop', dataTransfer({ files: [file] })));
    }

    it('refuses files that are not PDFs, briefly', () => {
      const fetch = mockFetch(async () => new Response(null, { status: 204 }));
      const { tile, status } = tileParts();

      drop(pdfFile('photo.png', 'image/png'));

      expect(fetch).not.toHaveBeenCalled();
      expect(tile.classList.contains('invalid')).toBe(true);
      expect(status.textContent).toBe('Not a PDF');

      vi.advanceTimersByTime(1300);
      expect(tile.classList.contains('invalid')).toBe(false);
      expect(status.textContent).toBe('');
    });

    it('posts the PDF to the printer and reports success, briefly', async () => {
      let respond!: (response: Response) => void;
      const fetch = mockFetch(() => new Promise((resolve) => (respond = resolve)));
      const { tile, status } = tileParts();

      drop(pdfFile('report.pdf'));

      expect(tile.classList.contains('sending')).toBe(true);
      expect(status.textContent).toBe('Sending…');
      const [url, init] = fetch.mock.calls[0];
      expect(url).toBe('/api/print/office%2F1');
      expect(init?.method).toBe('POST');
      const sent = (init?.body as FormData).get('file') as File;
      expect(sent.name).toBe('report.pdf');
      expect(sent.type).toBe('application/pdf');

      respond(new Response(null, { status: 204 }));
      await settle();
      expect(tile.classList.contains('sending')).toBe(false);
      expect(tile.classList.contains('success')).toBe(true);
      expect(status.textContent).toBe('Sent to printer');

      vi.advanceTimersByTime(3000);
      expect(tile.className).toBe('printer-tile');
      expect(status.textContent).toBe('');
    });

    it('shows the server error message when printing fails', async () => {
      mockFetch(async () => new Response('printer rejected the job', { status: 502 }));
      const { tile, status } = tileParts();

      drop(pdfFile());
      await settle();

      expect(tile.classList.contains('failure')).toBe(true);
      expect(status.textContent).toBe('printer rejected the job');
    });

    it('falls back to a generic message without one', async () => {
      mockFetch(async () => new Response('', { status: 500 }));
      drop(pdfFile());
      await settle();
      expect(tileParts().status.textContent).toBe('Printing failed');
    });

    it('falls back to a generic message if the error body is unreadable', async () => {
      const response = new Response('ignored', { status: 500 });
      vi.spyOn(response, 'text').mockRejectedValue(new Error('stream broke'));
      mockFetch(async () => response);

      drop(pdfFile());
      await settle();

      expect(tileParts().status.textContent).toBe('Printing failed');
    });

    it('reports a failure when the server is unreachable', async () => {
      mockFetch(async () => {
        throw new TypeError('Failed to fetch');
      });
      const { tile, status } = tileParts();

      drop(pdfFile());
      await settle();

      expect(tile.classList.contains('sending')).toBe(false);
      expect(tile.classList.contains('failure')).toBe(true);
      expect(status.textContent).toBe('Printing failed');
    });

    it('keeps a new result on screen when an earlier one would have cleared', async () => {
      mockFetch(async () => new Response(null, { status: 204 }));
      const { status } = tileParts();

      drop(pdfFile('photo.png', 'image/png'));
      vi.advanceTimersByTime(1000);
      drop(pdfFile());
      await settle();
      vi.advanceTimersByTime(500);

      expect(status.textContent).toBe('Sent to printer');
    });

    describe('following the job', () => {
      const accepted = (jobId: number | null) => mockFetch(async () => Response.json({ jobId }));
      const office = (overrides: Parameters<typeof status>[0]) => [printer({ id: 'office/1', status: status(overrides) })];

      it('shows its place in the queue until it has printed', async () => {
        accepted(7);
        const { tile, status: message } = tileParts();
        drop(pdfFile());
        await settle();
        expect(message.textContent).toBe('Sent to printer');

        render(app, office({ queued: 2, jobs: [{ id: 3, state: 'processing' }, { id: 7, state: 'pending' }] }));
        expect(message.textContent).toBe('Your job: 2nd of 2');

        render(app, office({ queued: 1, jobs: [{ id: 7, state: 'processing' }] }));
        expect(message.textContent).toBe('Printing your job');

        render(app, office({ state: 'stopped', queued: 1, jobs: [{ id: 7, state: 'processing-stopped' }] }));
        expect(message.textContent).toBe('Your job has stopped');
        expect(tile.classList.contains('waiting')).toBe(true);

        render(app, office({ finished: [{ id: 7, state: 'completed' }] }));
        expect(message.textContent).toBe('Printed');
        expect(tile.classList.contains('success')).toBe(true);

        vi.advanceTimersByTime(5000);
        expect(message.textContent).toBe('');
        render(app, office({ finished: [{ id: 7, state: 'completed' }] }));
        expect(message.textContent, 'a finished job is not shown again').toBe('');
      });

      it('picks up a job the printer reported before the upload finished', async () => {
        let respond!: (response: Response) => void;
        mockFetch(() => new Promise((resolve) => (respond = resolve)));
        drop(pdfFile());
        render(app, office({ queued: 1, jobs: [{ id: 9, state: 'pending' }] }));

        respond(Response.json({ jobId: 9 }));
        await settle();

        expect(tileParts().status.textContent).toBe('Your job: 1st of 1');
      });

      it('reports a cancelled job as a failure', async () => {
        accepted(7);
        drop(pdfFile());
        await settle();
        render(app, office({ finished: [{ id: 7, state: 'canceled' }] }));

        expect(tileParts().status.textContent).toBe('Your job was cancelled');
        expect(tileParts().tile.classList.contains('failure')).toBe(true);
      });

      it('stops following a job once another file is dropped', async () => {
        accepted(7);
        drop(pdfFile());
        await settle();
        drop(pdfFile('photo.png', 'image/png'));

        render(app, office({ queued: 1, jobs: [{ id: 7, state: 'pending' }] }));

        expect(tileParts().status.textContent).toBe('Not a PDF');
      });

      it('just confirms the upload when the printer gives no job id', async () => {
        accepted(null);
        drop(pdfFile());
        await settle();
        expect(tileParts().status.textContent).toBe('Sent to printer');
        vi.advanceTimersByTime(3000);
        expect(tileParts().status.textContent).toBe('');
      });
    });

    it('clears a failure after a while', async () => {
      mockFetch(async () => new Response('nope', { status: 404 }));
      const { tile } = tileParts();
      drop(pdfFile());
      await settle();
      vi.advanceTimersByTime(3000);
      expect(tile.className).toBe('printer-tile');
    });
  });
});

describe('connect', () => {
  beforeEach(() => {
    FakeEventSource.install();
  });

  it('subscribes to printer updates and renders each list', () => {
    const source = connect(app) as unknown as FakeEventSource;
    expect(source.url).toBe('/api/printers');

    source.emitPrinters([printer()]);
    expect(app.querySelectorAll('.printer-tile')).toHaveLength(1);

    source.emitPrinters([]);
    expect(app.querySelector('.empty-state')).not.toBeNull();
  });

  it('ignores malformed events', () => {
    const source = connect(app) as unknown as FakeEventSource;
    source.emitPrinters([printer()]);

    source.emit('{not json');
    source.emit('null');

    expect(app.querySelectorAll('.printer-tile')).toHaveLength(1);
  });
});

describe('start', () => {
  it('shows the empty state and connects', () => {
    FakeEventSource.install();
    start(app);
    expect(app.querySelector('.empty-state')).not.toBeNull();
    expect(FakeEventSource.latest().url).toBe('/api/printers');
  });
});
