/**
 * Test doubles for the browser APIs and server data the app uses.
 * @module
 */
import { vi } from 'vitest';
import type { Printer } from '../src/app';

/**
 * A printer as the server would list it.
 * @param overrides - Fields to change from the defaults.
 * @returns The printer.
 */
export function printer(overrides: Partial<Printer> = {}): Printer {
  return {
    id: 'p1',
    name: 'Office',
    uri: 'ipp://10.0.0.5:631/ipp/print',
    model: 'LaserJet 9000',
    formats: ['PDF', 'URF'],
    ...overrides,
  };
}

/** Stands in for the server's printer event stream. */
export class FakeEventSource {
  /** Every instance created since {@link FakeEventSource.install}. */
  static instances: FakeEventSource[] = [];
  onmessage: ((event: MessageEvent) => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  /**
   * Deliver one server-sent event.
   * @param data - The event's data.
   */
  emit(data: string): void {
    this.onmessage?.(new MessageEvent('message', { data }));
  }

  /**
   * Deliver a printer list, as the server does.
   * @param printers - The printers to send.
   */
  emitPrinters(printers: Printer[]): void {
    this.emit(JSON.stringify(printers));
  }

  /** Replace the global `EventSource` with this fake, forgetting earlier instances. */
  static install(): void {
    FakeEventSource.instances = [];
    vi.stubGlobal('EventSource', FakeEventSource);
  }

  /**
   * The most recently opened event stream.
   * @returns The latest instance.
   * @throws Error if none has been opened.
   */
  static latest(): FakeEventSource {
    const source = FakeEventSource.instances.at(-1);
    if (!source) throw new Error('no EventSource was opened');
    return source;
  }
}

/**
 * A small file, by default a PDF.
 * @param name - The file name.
 * @param type - The MIME type.
 * @returns The file.
 */
export function pdfFile(name = 'report.pdf', type = 'application/pdf'): File {
  return new File(['%PDF-1.4'], name, { type });
}

interface DataTransferInit {
  types?: string[];
  items?: { kind: string; type: string }[];
  files?: File[];
}

/**
 * A drag's data. jsdom has no DataTransfer, so this has only the parts
 * inkdrop reads.
 * @param init - The drag's types, items and files; by default a file drag
 * with no items or files.
 * @returns The data, with `dropEffect` "none".
 */
export function dataTransfer({ types = ['Files'], items = [], files = [] }: DataTransferInit = {}): DataTransfer {
  return { types, items, files, dropEffect: 'none' } as unknown as DataTransfer;
}

/**
 * A drag event. jsdom has no DragEvent, so `dataTransfer` is attached.
 * @param type - The event type, e.g. `dragover`.
 * @param transfer - The drag's data, if any.
 * @returns A bubbling, cancelable event.
 */
export function dragEvent(type: string, transfer?: DataTransfer): Event {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperty(event, 'dataTransfer', { value: transfer });
  return event;
}

/**
 * Choose files in a (hidden) file input, as the native picker would.
 * @param input - The file input.
 * @param files - The files to choose.
 */
export function chooseFiles(input: HTMLInputElement, files: File[]): void {
  Object.defineProperty(input, 'files', { value: files, configurable: true });
  input.dispatchEvent(new Event('change'));
}

/**
 * Let pending promise callbacks (fetch, response.text()) run.
 * @returns A promise that resolves once they have.
 */
export async function settle(): Promise<void> {
  for (let i = 0; i < 20; i++) {
    await Promise.resolve();
  }
}
