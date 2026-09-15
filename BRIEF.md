This is the brief.

* This will be a Linux web service, using Rust, binding to an unpriviledged port. 
* The browser-side shall be implemented in TypeScript, and use Vite to bundle it.
* Use mDNS to find IPP printers on the network that can accept PDFs (this is important)
* Use SQLite, if any persistence is reqired. 
* the web service will present a single page application (SPA), displaying super big friendly printer emoji for each capable printer, with the name underneath. If none are found, it will show a super big friendly "thinking" emoji, and explainatory text that no printer has been found. 
* When the user drags a document over the printer icon/emoji the mouse icon shall reflect the suitability of that document: yes for PDF and no for anything else. 
* When the user successfully drags a PDF to the printer icon, the app shall use IPP to print that PDF. 