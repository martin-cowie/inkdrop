/**
 * Entry point: starts the app in the page's `#app` element.
 * @module
 */
import './style.css';
import { start } from './app';

start(document.querySelector<HTMLDivElement>('#app')!);
