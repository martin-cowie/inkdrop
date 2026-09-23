import { expect, it } from 'vitest';
import { FakeEventSource } from '../tests/helpers';

it('starts the app in #app', async () => {
  FakeEventSource.install();
  document.body.innerHTML = '<div id="app"></div>';

  await import('./main');

  expect(document.querySelector('#app .empty-state')).not.toBeNull();
  expect(FakeEventSource.latest().url).toBe('/api/printers');
});
