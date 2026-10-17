# Local browser

The browser and your server run inside this workshop. Loopback works even
though external networking is disabled. Bind servers to `127.0.0.1`.

```python
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.launch(args=["--no-sandbox"])
    page = browser.new_page()
    page.goto("file:///work/index.html")
    print(page.locator("body").inner_text())
    page.screenshot(path="/work/screenshot.png")
    browser.close()
```

For a local server, launch `python -m http.server 8000 --bind 127.0.0.1`
in the background with output redirected to a file, then open
`http://127.0.0.1:8000` from Playwright. Servers stop at session reset.
