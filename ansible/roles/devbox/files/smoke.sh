#!/usr/bin/env bash
set -euo pipefail
source /etc/devbox/environment
if [[ $(id -un) != "$DEVBOX_USER" ]]; then echo "Run this as $DEVBOX_USER" >&2; exit 1; fi
export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}
if [[ $DEVBOX_PASSWORDLESS_SUDO == 1 ]]; then sudo -n true; else sudo -v; fi
while IFS= read -r tool; do
  command -v "$tool" >/dev/null || { echo "Missing expected command: $tool" >&2; exit 1; }
done </etc/devbox/expected-commands
node --version
npm --version
if command -v docker >/dev/null; then
  docker compose version
  docker buildx version
fi
devbox-wait-display
out=$(mktemp -d /srv/agent/artifacts/smoke.XXXXXX)
glxinfo -B >"$out/opengl.txt"
vulkaninfo --summary >"$out/vulkan.txt"
if [[ $DEVBOX_SOFTWARE_RENDERING == 1 ]]; then
  grep -qi llvmpipe "$out/opengl.txt"
  grep -Eqi 'llvmpipe|lavapipe' "$out/vulkan.txt"
fi
# Independent virtual display for headless GUI/test runners.
xvfb-run -a -s '-screen 0 1280x720x24 +extension GLX' glxinfo -B >"$out/xvfb-opengl.txt"
if [[ $DEVBOX_SOFTWARE_RENDERING == 1 ]]; then
  grep -qi llvmpipe "$out/xvfb-opengl.txt"
fi
devbox-desktop screenshot --output "$out/desktop.png"
xdpyinfo | grep 'dimensions:'
python - "$out" <<'PY'
import sys
from pathlib import Path
from docx import Document
from openpyxl import Workbook
from reportlab.pdfgen import canvas
p = Path(sys.argv[1])
d = Document(); d.add_paragraph('Devbox smoke test'); d.save(p / 'test.docx')
w = Workbook(); w.active['A1'] = 'Devbox smoke test'; w.save(p / 'test.xlsx')
c = canvas.Canvas(str(p / 'test.pdf')); c.drawString(50, 750, 'Devbox smoke test'); c.save()
PY
libreoffice --headless "-env:UserInstallation=file://$out/lo-profile" --convert-to pdf --outdir "$out/rendered" "$out/test.docx"
test -s "$out/rendered/test.pdf"
pdftoppm -singlefile -png -scale-to 600 "$out/test.pdf" "$out/pdf"
test -s "$out/pdf.png"
browser_check() {
  python - "$out" <<'PY'
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    browser=p.chromium.launch(headless=False, chromium_sandbox=True)
    page=browser.new_page()
    page.set_content('<h1>Devbox browser smoke test</h1><input id="test">')
    page.locator('#test').fill('sandbox enabled')
    assert page.locator('#test').input_value() == 'sandbox enabled'
    page.screenshot(path=str(Path(sys.argv[1])/'browser.png'))
    browser.close()
PY
}
skipped=()
if /opt/devbox-tools/venv/bin/python -c 'import playwright' 2>/dev/null; then
  if [[ $DEVBOX_PROFILE != container ]]; then
    browser_check
  elif ! browser_check 2>"$out/browser-error.txt"; then
    # Docker's default seccomp profile blocks the user namespaces Chromium's
    # sandbox needs. The sandbox stays on; the check is reported, not hidden.
    skipped+=("sandboxed browser launch (see $out/browser-error.txt)")
  fi
fi
if command -v claude >/dev/null; then claude --version; fi
if command -v codex >/dev/null; then codex --version; fi
if command -v pi >/dev/null; then pi --version; fi
if command -v t3 >/dev/null; then t3 --version; fi
curl -fsS "http://127.0.0.1:6080/vnc.html" >/dev/null
# Cockpit serves a self-signed certificate until one is installed.
if [[ ${DEVBOX_COCKPIT:-0} == 1 ]]; then curl -kfsS https://127.0.0.1:9090/ >/dev/null; fi
if command -v t3 >/dev/null; then curl -fsS http://127.0.0.1:3773/ >/dev/null; fi
printf 'Smoke checks passed. Outputs: %s\n' "$out"
for item in "${skipped[@]}"; do printf 'Skipped in this container: %s\n' "$item"; done
printf 'Agent authentication and a T3-launched sudo test remain interactive checks.\n'
