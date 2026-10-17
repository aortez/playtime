"""Self-contained report styling and controls; no network dependencies."""

STYLE = """
:root{color-scheme:light dark;--bg:#f5f6f8;--paper:#fff;--ink:#202b3a;--muted:#59677b;
--line:#dce2e9;--accent:#136e69;--soft:#edf6f4;--warn:#94450a;--danger:#b02b39;
font:15px/1.55 system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink)}
main{max-width:1120px;margin:auto;padding:38px 28px 70px}a{color:var(--accent);text-underline-offset:3px}
h1{font-size:2rem;letter-spacing:-.04em;line-height:1.2;margin:12px 0}h2{font-size:1.25rem;margin:0 0 12px}
h3{font-size:1rem;margin:0}p{margin:8px 0 16px}.muted,.meta{color:var(--muted)}
.eyebrow,.text-label{font-size:.72rem;font-weight:700;letter-spacing:.09em;text-transform:uppercase;color:var(--muted)}
.topline,.row,.step-heading{display:flex;align-items:center;justify-content:space-between;gap:12px;flex-wrap:wrap}
.badge{display:inline-block;padding:3px 9px;border:1px solid var(--line);border-radius:5px;font-size:.75rem;font-weight:600}
.badge.good{color:var(--accent);background:var(--soft)}.badge.warn{color:var(--warn)}.badge.bad,.error{color:var(--danger)}
.stats{display:grid;grid-template-columns:repeat(5,minmax(0,1fr));gap:12px;margin:24px 0}
.stat{padding:16px;background:var(--paper);border:1px solid var(--line);border-radius:9px;min-width:0}
.stat strong{display:block;font-size:1.25rem;overflow-wrap:anywhere}.stat span{font-size:.78rem;color:var(--muted)}
.panel,.session,.run-card{background:var(--paper);border:1px solid var(--line);border-radius:10px;padding:22px;margin:20px 0}
.panel p:last-child{margin-bottom:0}.links{font-size:.84rem;display:flex;gap:16px;flex-wrap:wrap}
pre{font: .84rem/1.55 ui-monospace,SFMono-Regular,Consolas,monospace;white-space:pre-wrap;overflow-wrap:anywhere;margin:8px 0}
.text-block{margin:12px 0}.text-block>pre,.text-block details>pre{padding:12px 14px;background:var(--bg);border-radius:6px}
.text-block details{margin:6px 0}.text-block:has(>details[open])>.preview{display:none}
summary{cursor:pointer;color:var(--accent);font-size:.8rem}summary:hover{text-decoration:underline}
.raw{margin-top:14px;border-top:1px solid var(--line);padding-top:10px}.raw pre{max-height:65vh;overflow:auto;background:var(--bg);padding:14px}
.toolbar{position:sticky;top:0;z-index:2;background:var(--paper);border:1px solid var(--line);border-radius:9px;padding:12px 16px;margin:28px 0 16px}
.controls{display:none;gap:3px;background:var(--bg);padding:3px;border-radius:7px}.js .controls{display:flex}
button{font:inherit;font-size:.8rem;cursor:pointer;border:1px solid transparent;background:transparent;color:var(--muted);padding:6px 12px;border-radius:5px}
button[aria-pressed=true]{background:var(--paper);border-color:var(--line);color:var(--ink);box-shadow:0 1px 2px #0001}
button:focus-visible,a:focus-visible,summary:focus-visible{outline:3px solid var(--accent);outline-offset:3px}
.session-nav{display:flex;gap:8px;flex-wrap:wrap;margin:12px 0}.session-nav a{padding:3px 9px;border:1px solid var(--line);border-radius:5px;font-size:.8rem;text-decoration:none}
.session{padding:0;scroll-margin-top:150px;overflow:hidden}.session-head{padding:20px 22px;background:var(--soft);border-bottom:1px solid var(--line)}
.session-head h2{margin:0}.memory-grid{display:grid;grid-template-columns:1fr 1fr;gap:20px;padding:4px 22px 14px;border-bottom:1px solid var(--line)}
.memory-grid>div{min-width:0}.memory-grid pre{font-family:inherit;font-size:.86rem}
.steps{padding:0 22px}.step{padding:20px 0;border-bottom:1px solid var(--line)}.step:last-child{border:0}
.step-heading{justify-content:flex-start}.step-heading .meta{margin-left:auto;font-size:.76rem}
.step-number{font-size:.7rem;letter-spacing:.04em;color:var(--muted);min-width:44px}
.step-title{font-weight:650;font-size:.91rem;overflow-wrap:anywhere;flex:1;min-width:140px}
.tool{border-left:3px solid var(--line);padding:2px 0 2px 16px;margin-top:16px}.tool.failed{border-color:var(--danger)}
.tool .row{font-size:.85rem}.tool-status{font-size:.75rem;color:var(--muted)}.tool-status.bad{color:var(--danger)}
.notice{padding:10px 14px;background:var(--bg);border-radius:6px;font-size:.85rem;margin:12px 0}
.session-footer{padding:14px 22px;border-top:1px solid var(--line);font-size:.85rem}
body[data-view=compact] .step-body{display:none}body[data-view=compact] .step{padding:13px 0}
body[data-view=compact] .memory-grid{display:none}body[data-view=compact] .session-footer .text-block{display:none}
.artifact-list{list-style:none;padding:0;margin:0}.artifact-list li{padding:12px 0;border-bottom:1px solid var(--line)}
.artifact-list li:last-child{border:0}.artifact-list a{overflow-wrap:anywhere}.artifact-list .meta{white-space:nowrap;font-size:.8rem}
.artifact-source{margin-top:6px}.artifact-source pre{max-height:32rem;overflow:auto;background:var(--bg);padding:14px;border-radius:6px}
#artifacts,#visual-outputs{scroll-margin-top:140px}.image-grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:18px}
.image-card{min-width:0;margin:0;border:1px solid var(--line);border-radius:9px;overflow:hidden;background:var(--paper)}
.image-open{position:relative;display:block;width:100%;height:260px;padding:16px;border:0;border-radius:0;background:var(--bg);cursor:default}
.image-open img{display:block;width:100%;height:100%;object-fit:contain}.js .image-open{cursor:zoom-in}
.image-open:focus-visible{outline-offset:-4px}.image-zoom{display:none;position:absolute;bottom:10px;right:10px;padding:3px 9px;background:var(--paper);border:1px solid var(--line);border-radius:5px;font-size:.72rem;color:var(--ink)}
.js .image-zoom{display:block}.image-open:hover .image-zoom{border-color:var(--accent);color:var(--accent)}
.image-card figcaption{padding:13px 15px}.image-card figcaption>strong{display:block;font-size:.86rem;overflow-wrap:anywhere}
.image-meta{display:flex;align-items:center;flex-wrap:wrap;gap:8px;margin:8px 0;color:var(--muted);font-size:.74rem}.image-meta .badge{font-size:.64rem;padding:1px 6px}
.image-card .links{font-size:.75rem;gap:12px}.image-unavailable{display:flex;align-items:center;justify-content:center;min-height:120px;padding:24px;background:var(--bg);color:var(--muted);font-size:.85rem;text-align:center}
.session-images{padding:20px 22px;border-bottom:1px solid var(--line)}.session-images>p{font-size:.8rem;margin:4px 0 14px}
.session-images .image-open{height:220px}#image-viewer{width:min(96vw,1320px);max-width:none;max-height:96vh;padding:20px;border:1px solid var(--line);border-radius:12px;background:var(--paper);color:var(--ink)}
#image-viewer::backdrop{background:#09111ed9}#image-title{margin:0;font-size:1.1rem;overflow-wrap:anywhere;flex:1;min-width:0}
#image-caption{font-size:.8rem;margin:6px 0 14px}#image-stage{overflow:auto;max-height:70vh;background:var(--bg);border-radius:6px}
#image-stage img{display:block;margin:auto;max-width:100%;max-height:70vh;object-fit:contain}#image-stage.actual-size img{max-width:none;max-height:none}
.image-actions{margin-top:14px;font-size:.85rem}[data-close-preview],[data-image-size]{border-color:var(--line);background:var(--bg);color:var(--ink)}
.run-card{display:block;text-decoration:none;color:var(--ink)}.run-card:hover{border-color:var(--accent)}
.run-card .meta{font-size:.83rem;margin-top:8px}.run-card h2{margin:8px 0}.run-card pre{font-family:inherit;color:var(--muted)}
.empty{padding:30px;text-align:center;color:var(--muted)}footer{margin-top:30px;font-size:.8rem;color:var(--muted)}
@media(prefers-color-scheme:dark){:root{--bg:#151b24;--paper:#1c2531;--ink:#e2e9f2;--muted:#a7b3c4;--line:#334254;--accent:#74cec2;--soft:#1b3335;--warn:#e7b16c;--danger:#ff98a3}}
@media(max-width:700px){main{padding:22px 14px}.stats{grid-template-columns:repeat(2,minmax(0,1fr))}.memory-grid{grid-template-columns:1fr;gap:0}
.toolbar{padding:10px}.toolbar .row{gap:8px}button{padding:6px 9px}.step-heading .meta{width:100%;margin-left:56px}.panel{padding:16px}
.session-head,.session-footer{padding:16px}.steps{padding:0 16px}.memory-grid{padding:0 16px 12px}}
@media(max-width:600px){.image-grid{grid-template-columns:1fr}.image-open{height:240px}.session-images{padding:16px}#image-viewer{padding:14px}}
@media print{.toolbar{position:static}.controls{display:none!important}main{max-width:none;padding:0}.session{break-inside:auto}.step,.image-card{break-inside:avoid}.image-zoom{display:none!important}dialog{display:none!important}}
"""

SCRIPT = """
document.documentElement.classList.add('js');
document.querySelectorAll('[data-set-view]').forEach(button => {
  button.addEventListener('click', () => {
    const view = button.dataset.setView;
    document.body.dataset.view = view;
    document.querySelectorAll('[data-set-view]').forEach(item => {
      item.setAttribute('aria-pressed', String(item === button));
    });
    document.querySelectorAll('main details').forEach(item => {
      item.open = view === 'full';
    });
  });
});
const viewer = document.querySelector('#image-viewer');
if (viewer && typeof viewer.showModal === 'function') {
  const stage = viewer.querySelector('#image-stage');
  const sizeButton = viewer.querySelector('[data-image-size]');
  document.querySelectorAll('[data-image-preview]').forEach(button => {
    button.addEventListener('click', () => {
      const original = button.querySelector('img');
      const image = original.cloneNode();
      image.removeAttribute('loading');
      stage.replaceChildren(image);
      stage.classList.remove('actual-size');
      sizeButton.textContent = 'Actual size';
      sizeButton.setAttribute('aria-pressed', 'false');
      viewer.querySelector('#image-title').textContent = button.dataset.name;
      const caption = viewer.querySelector('#image-caption');
      caption.textContent = button.dataset.caption;
      const dimensions = () => {
        if (image.naturalWidth) {
          caption.textContent = `${button.dataset.caption} · ${image.naturalWidth} × ${image.naturalHeight} pixels`;
        }
      };
      image.addEventListener('load', dimensions, {once: true});
      dimensions();
      const download = viewer.querySelector('[data-image-download]');
      download.href = button.dataset.download || original.src;
      download.download = button.dataset.name.split('/').pop();
      viewer.showModal();
    });
  });
  sizeButton.addEventListener('click', () => {
    const actual = stage.classList.toggle('actual-size');
    sizeButton.textContent = actual ? 'Fit to window' : 'Actual size';
    sizeButton.setAttribute('aria-pressed', String(actual));
  });
  viewer.querySelector('[data-close-preview]').addEventListener('click', () => viewer.close());
  viewer.addEventListener('click', event => {
    if (event.target === viewer) {
      const rect = viewer.getBoundingClientRect();
      if (event.clientX < rect.left || event.clientX > rect.right ||
          event.clientY < rect.top || event.clientY > rect.bottom) viewer.close();
    }
  });
  viewer.addEventListener('close', () => stage.replaceChildren());
}
"""
