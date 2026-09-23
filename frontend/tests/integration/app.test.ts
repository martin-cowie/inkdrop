/**
 * The whole page — index.html plus main.ts — against a fake backend that
 * answers like the Rust server: printers arrive over the event stream, and
 * uploads are accepted or refused per printer.
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import indexHtml from '../../index.html?raw';
import type { Printer } from '../../src/app';
import { FakeEventSource, chooseFiles, dataTransfer, dragEvent, pdfFile, printer, settle } from '../helpers';

/** Uploads received, and how each printer responds. */
const backend = {
  uploads: [] as { printerId: string; file: File }[],
  responses: new Map<string, Response>(),

  async fetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
    const match = /^\/api\/print\/(.+)$/.exec(String(input));
    if (!match || init?.method !== 'POST') {
      return new Response('not found', { status: 404 });
    }
    const printerId = decodeURIComponent(match[1]);
    backend.uploads.push({ printerId, file: (init.body as FormData).get('file') as File });
    return backend.responses.get(printerId) ?? new Response('printer not found', { status: 404 });
  },
};

const office = printer({ id: 'office', name: 'Office', formats: ['PDF'] });
const attic = printer({ id: 'attic', name: 'Attic', model: null, formats: ['PWG-Raster'] });

let events: FakeEventSource;

function tile(name: string) {
  const tiles = [...document.querySelectorAll<HTMLDivElement>('.printer-tile')];
  const found = tiles.find((t) => t.querySelector('.name')?.textContent === name);
  if (!found) throw new Error(`no tile for ${name}`);
  return {
    element: found,
    status: () => found.querySelector('.status')!.textContent,
    input: found.querySelector<HTMLInputElement>('.file-input')!,
  };
}

function tileNames(): string[] {
  return [...document.querySelectorAll('.printer-tile .name')].map((n) => n.textContent ?? '');
}

function publish(printers: Printer[]) {
  events.emitPrinters(printers);
}

beforeAll(async () => {
  FakeEventSource.install();
  const page = new DOMParser().parseFromString(indexHtml, 'text/html');
  document.body.innerHTML = page.body.innerHTML;

  await import('../../src/main');
  events = FakeEventSource.latest();
});

beforeEach(() => {
  vi.stubGlobal('fetch', vi.fn(backend.fetch));
  backend.uploads = [];
  backend.responses = new Map([
    ['office', new Response(null, { status: 204 })],
    ['attic', new Response(null, { status: 204 })],
  ]);
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
});

afterEach(() => {
  vi.useRealTimers();
});

describe('inkdrop page', () => {
  it('starts empty and subscribes to printer updates', () => {
    expect(document.querySelector('#app .brand')).not.toBeNull();
    expect(document.querySelector('.empty-state')).not.toBeNull();
    expect(events.url).toBe('/api/printers');
  });

  it('lists printers as the server announces them', () => {
    publish([attic, office]);
    expect(tileNames()).toEqual(['Attic', 'Office']);
    expect(tile('Office').element.querySelector('.badge')?.textContent).toBe('PDF');
    expect(tile('Attic').element.querySelector('.model')).toBeNull();

    publish([office]);
    expect(tileNames()).toEqual(['Office']);
  });

  it('prints a PDF dropped on a printer', async () => {
    publish([attic, office]);
    const target = tile('Attic');

    target.element.dispatchEvent(dragEvent('dragenter', dataTransfer()));
    target.element.dispatchEvent(
      dragEvent('dragover', dataTransfer({ items: [{ kind: 'file', type: 'application/pdf' }] })),
    );
    expect(target.element.classList.contains('drag-ok')).toBe(true);

    target.element.dispatchEvent(dragEvent('drop', dataTransfer({ files: [pdfFile('minutes.pdf')] })));
    expect(target.status()).toBe('Printing…');
    await settle();

    expect(backend.uploads.map((u) => [u.printerId, u.file.name])).toEqual([['attic', 'minutes.pdf']]);
    expect(target.status()).toBe('Sent to printer');
    expect(target.element.classList.contains('success')).toBe(true);

    vi.advanceTimersByTime(3000);
    expect(target.status()).toBe('');
  });

  it('prints a PDF chosen with the file picker', async () => {
    publish([office]);
    const target = tile('Office');
    const openPicker = vi.spyOn(target.input, 'click').mockImplementation(() => {});

    target.element.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', cancelable: true }));
    expect(openPicker).toHaveBeenCalled();
    chooseFiles(target.input, [pdfFile('invoice.pdf', '')]);
    await settle();

    expect(backend.uploads.map((u) => u.file.name)).toEqual(['invoice.pdf']);
    expect(target.status()).toBe('Sent to printer');
  });

  it('refuses a file that is not a PDF without contacting the server', () => {
    publish([office]);
    const target = tile('Office');

    target.element.dispatchEvent(
      dragEvent('dragover', dataTransfer({ items: [{ kind: 'file', type: 'image/jpeg' }] })),
    );
    expect(target.element.classList.contains('drag-bad')).toBe(true);
    target.element.dispatchEvent(
      dragEvent('drop', dataTransfer({ files: [pdfFile('holiday.jpg', 'image/jpeg')] })),
    );

    expect(target.status()).toBe('Not a PDF');
    expect(backend.uploads).toEqual([]);
  });

  it("shows the server's reason when a print fails", async () => {
    backend.responses.set('office', new Response('printer no longer supports PDF', { status: 422 }));
    publish([office]);

    tile('Office').element.dispatchEvent(dragEvent('drop', dataTransfer({ files: [pdfFile()] })));
    await settle();

    expect(tile('Office').status()).toBe('printer no longer supports PDF');
    expect(tile('Office').element.classList.contains('failure')).toBe(true);
  });

  it('reports printers that vanished before the upload arrived', async () => {
    backend.responses.delete('office');
    publish([office]);

    tile('Office').element.dispatchEvent(dragEvent('drop', dataTransfer({ files: [pdfFile()] })));
    await settle();

    expect(tile('Office').status()).toBe('printer not found');
  });

  it('returns to the empty state when every printer goes away', () => {
    publish([office]);
    publish([]);
    expect(document.querySelector('.printer-tile')).toBeNull();
    expect(document.querySelector('.empty-state')).not.toBeNull();
  });
});
