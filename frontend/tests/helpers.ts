import { vi } from 'vitest';
import type { Printer } from '../src/app';

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
  static instances: FakeEventSource[] = [];
  onmessage: ((event: MessageEvent) => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  /** Deliver one server-sent event. */
  emit(data: string): void {
    this.onmessage?.(new MessageEvent('message', { data }));
  }

  emitPrinters(printers: Printer[]): void {
    this.emit(JSON.stringify(printers));
  }

  static install(): void {
    FakeEventSource.instances = [];
    vi.stubGlobal('EventSource', FakeEventSource);
  }

  static latest(): FakeEventSource {
    const source = FakeEventSource.instances.at(-1);
    if (!source) throw new Error('no EventSource was opened');
    return source;
  }
}

export function pdfFile(name = 'report.pdf', type = 'application/pdf'): File {
  return new File(['%PDF-1.4'], name, { type });
}

interface DataTransferInit {
  types?: string[];
  items?: { kind: string; type: string }[];
  files?: File[];
}

/** jsdom has no DataTransfer, so build the parts inkdrop reads. */
export function dataTransfer({ types = ['Files'], items = [], files = [] }: DataTransferInit = {}): DataTransfer {
  return { types, items, files, dropEffect: 'none' } as unknown as DataTransfer;
}

/** A drag event; jsdom has no DragEvent, so `dataTransfer` is attached. */
export function dragEvent(type: string, transfer?: DataTransfer): Event {
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperty(event, 'dataTransfer', { value: transfer });
  return event;
}

/** Choose `files` in a (hidden) file input, as the native picker would. */
export function chooseFiles(input: HTMLInputElement, files: File[]): void {
  Object.defineProperty(input, 'files', { value: files, configurable: true });
  input.dispatchEvent(new Event('change'));
}

/** Let pending promise callbacks (fetch, response.text()) run. */
export async function settle(): Promise<void> {
  for (let i = 0; i < 20; i++) {
    await Promise.resolve();
  }
}
