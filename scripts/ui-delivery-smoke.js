// Explicit debug-only native own-window test. Never assign the expected payload
// to the input: only real input events from the production paste path count.
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
  const checks = {
    disabled_before_probe: false,
    native_input_readback: false,
    enabled_after_probe: false,
    operation_finished: false,
  };
  try {
    const deadline = Date.now() + 10000;
    let before;
    while (Date.now() < deadline) {
      try {
        before = await invoke('snapshot');
        if (before.capabilities.focus_events_connected &&
            document.getElementById('begin-delivery-probe')) break;
      } catch { /* bounded startup polling */ }
      await delay(100);
    }
    checks.disabled_before_probe = before?.capabilities.automatic_delivery_verified === false;
    document.querySelector('[data-panel="settings"]').click();
    document.getElementById('open-delivery-probe').click();
    const input = document.getElementById('delivery-probe-input');
    let nativeInputEvents = 0;
    let nativePasteEvents = 0;
    input.addEventListener('input', event => { if (event.isTrusted) nativeInputEvents++; });
    input.addEventListener('paste', event => { if (event.isTrusted) nativePasteEvents++; });
    await delay(300);
    document.getElementById('begin-delivery-probe').click();
    const finished = Date.now() + 8000;
    while (Date.now() < finished) {
      const after = await invoke('snapshot');
      checks.operation_finished = !after.operation_busy;
      if (after.capabilities.automatic_delivery_verified) {
        checks.native_input_readback = nativeInputEvents > 0 && nativePasteEvents > 0 && input.value ===
          '语音输入粘贴验证 2026\nVoice input paste probe: /tmp/example.txt';
        checks.enabled_after_probe = true;
        checks.operation_finished = !after.operation_busy;
        break;
      }
      await delay(100);
    }
    checks.trusted_input_seen = nativeInputEvents > 0;
    checks.trusted_paste_seen = nativePasteEvents > 0;
    checks.dom_input_focused = document.activeElement === input;
    checks.document_focused = document.hasFocus();
    const notice = document.getElementById('delivery-probe-status').textContent;
    checks.ui_dispatch_failed = notice.includes('无法向本应用验证框粘贴');
    checks.ui_mismatch = notice.includes('验证文字或目标已改变');
  } catch { /* report only fixed booleans, never user or native output */ }
  await invoke('ui_smoke_report', { report: checks });
})();
