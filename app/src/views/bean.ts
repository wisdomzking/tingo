/* 录音豆（soundcore Work / D3200）：发现设备 → 绑定固定设备 → 看设备信息 → 列离线录音 → 导入到 tingo。
   仅用于连接自己的录音硬件；音频导出后走本地识别与纪要，不经过任何云端。 */

import { t } from "../i18n";
import { api } from "../api";
import { $, esc, errorMessage, fmt, ICON, main, toast } from "../dom";

interface BeanDevice {
  id?: string;
  name?: string;
  rssi?: number;
  advName?: string;
  productCode?: string;
  serviceUuid?: string;
  macAddress?: string;
  isBoundAdvertised?: boolean;
  /** 扫描后补读：设备信息帧才有的稳定序列号，用来区分同型号设备。 */
  serial?: string;
  battery?: number;
  probeError?: string;
  probing?: boolean;
}

interface BeanFile {
  fileId: number;
  title?: string;
  durationMs?: number;
  payloadBytes?: number;
  endTime?: number;
  frameCount?: number;
}

interface BeanBinding {
  bound?: boolean;
  target?: string;
  name?: string;
  mac?: string;
  serial?: string;
  boundAt?: string;
  displayName?: string;
}

let selected = "";
let devices: BeanDevice[] = [];
let binding: BeanBinding | null = null;
/** 是否正在逐台读序列号：绑定开始时取消剩余探测，避免两路 BLE 并发。 */
let probeActive = false;
let probeCancel = false;
const probeIdleWaiters: Array<() => void> = [];

function selectedDevice(): BeanDevice | undefined {
  return devices.find((d) => deviceTarget(d) === selected);
}

/** 绑定按钮只受「所选设备」的探测状态影响。
 *  绝不能等所有设备都读完——附近有一台读不到（超时）就会把整页按钮永久锁死。 */
function syncBindButton(): void {
  if (binding?.bound) return;
  const bind = $<HTMLButtonElement>("#beanBindBtn");
  const reading = !!selectedDevice()?.probing;
  bind.disabled = !selected || reading;
  bind.textContent = reading ? t("读取序列号中…", "Reading serial…") : t("绑定此设备", "Bind this device");
}

function waitProbeIdle(timeoutMs = 10000): Promise<void> {
  if (!probeActive) return Promise.resolve();
  return new Promise((resolve) => {
    const timer = window.setTimeout(done, timeoutMs);
    function done(): void {
      window.clearTimeout(timer);
      const i = probeIdleWaiters.indexOf(done);
      if (i >= 0) probeIdleWaiters.splice(i, 1);
      resolve();
    }
    probeIdleWaiters.push(done);
  });
}

/** 绑定后自动同步录音：按设备序列号在本机记住已导入的文件 ID，避免重复导入。 */
let autoSyncRunning = false;
let autoSyncDone = false;

function syncStorageKey(): string {
  const serial = binding?.serial || selectedDevice()?.serial || "";
  return `tingo.bean.synced.${serial || selected || "unknown"}`;
}

function readSyncedIds(key: string): number[] {
  try {
    const raw = localStorage.getItem(key);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.filter((x): x is number => typeof x === "number") : [];
  } catch {
    return [];
  }
}

function writeSyncedIds(key: string, ids: number[]): void {
  try {
    localStorage.setItem(key, JSON.stringify(ids));
  } catch {
    // 隐私模式下 localStorage 可能不可用；只影响去重，不影响导入本身。
  }
}

function setSyncStatus(text: string): void {
  const el = document.getElementById("beanSyncStatus");
  if (el) el.textContent = text;
}

const cardStyle =
  "background:var(--card,rgba(255,255,255,.72));border:1px solid rgba(31,75,255,.12);border-radius:16px;padding:16px 18px;margin-bottom:14px;box-shadow:0 6px 24px rgba(16,24,64,.05)";
const muted = "opacity:.68;font-size:12px";

/** 设备唯一标识：优先厂商广播里的真实 MAC。
 *  绝不能只用名字——两台同型号录音豆广播名一样（soundcore Work），
 *  按名字取 target 会让两台一起高亮、连接时也随机连到其中一台。 */
function deviceTarget(d: BeanDevice): string {
  return d.macAddress || d.id || d.name || d.advName || "";
}

/** 列表里展示用的名字；同名时补 MAC 尾巴，避免用户分不清。 */
function deviceLabel(d: BeanDevice): string {
  return d.name || d.advName || d.productCode || t("未命名设备", "Unnamed device");
}

function boundName(): string {
  if (!binding) return "";
  return binding.name || binding.displayName || binding.target || t("录音豆", "Recorder bean");
}

/** BLE 操作在 WinRT 下偶发挂起（尤其两路并发时）；给每个录音豆请求加超时，避免按钮永久卡在"XX中"。 */
function withBeanTimeout<T>(promise: Promise<T>, action: string): Promise<T> {
  const ms = 45000;
  return Promise.race([
    promise,
    new Promise<never>((_, reject) =>
      window.setTimeout(
        () => reject(new Error(t(`${action}耗时过长，请确认蓝牙正常后重试`, `${action} timed out — check Bluetooth and retry`))),
        ms,
      ),
    ),
  ]);
}

export function renderBean(): void {
  selected = "";
  devices = [];
  binding = null;
  autoSyncRunning = false;
  autoSyncDone = false;
  main().innerHTML = `
    <div class="page bean-page">
      <div class="hero-head">
        <span class="hero-tag"><b></b>${t("蓝牙直连 · 录音不出本机", "Bluetooth direct · audio never leaves your PC")}</span>
        <h1>${t("录音豆", "Recorder bean")}</h1>
        <p>${t("绑定一台固定的录音豆，把设备里的录音导进 tingo，自动生成转写和纪要。解绑后可再绑定其他设备。", "Bind one recorder, import its offline recordings into tingo, and get transcripts plus minutes automatically. Unbind to pair another device.")}</p>
      </div>
      <div style="${cardStyle}">
        <div style="display:flex;align-items:center;gap:12px;flex-wrap:wrap">
          <span class="ico" style="width:38px;height:38px;display:grid;place-items:center;border-radius:12px;background:rgba(31,75,255,.1);color:var(--blue)">${ICON.mic}</span>
          <div style="flex:1;min-width:180px">
            <b>${t("连接状态", "Connection")}</b>
            <div id="beanStatus" style="${muted}">${t("正在检查…", "Checking…")}</div>
          </div>
          <button class="btn" id="beanSelfBtn">${t("协议自检", "Self-test")}</button>
          <button class="btn primary" id="beanScanBtn" style="display:none">${t("扫描设备", "Scan devices")}</button>
          <button class="btn primary" id="beanBindBtn" style="display:none">${t("绑定此设备", "Bind this device")}</button>
          <button class="btn" id="beanUnbindBtn" style="display:none">${t("解绑", "Unbind")}</button>
        </div>
        <div id="beanBindingBar" style="margin-top:12px"></div>
      </div>
      <div id="beanReport"></div>
      <div id="beanDevices"></div>
      <div id="beanDetail"></div>
      <div style="${muted};margin-top:18px">
        ${t("仅支持与自有录音豆硬件互操作。请先在系统蓝牙设置里配对，并确保设备未被其他程序占用。", "Only for interoperating with your own recorder hardware. Pair it in system Bluetooth settings first and make sure no other app is using it.")}
      </div>
    </div>`;

  $("#beanScanBtn").onclick = () => void scanDevices();
  $("#beanBindBtn").onclick = () => void bindDevice();
  $("#beanUnbindBtn").onclick = () => void unbindDevice();
  $("#beanSelfBtn").onclick = () => void selfTest();
  void loadStatus();
}

/** 按绑定状态切换按钮、提示条和设备列表，避免界面上出现"已绑定还能绑别的"的错觉。 */
function applyBindingUI(): void {
  const isBound = !!binding?.bound;
  const scan = $<HTMLButtonElement>("#beanScanBtn");
  const bind = $<HTMLButtonElement>("#beanBindBtn");
  const unbind = $<HTMLButtonElement>("#beanUnbindBtn");
  scan.style.display = isBound ? "none" : "";
  bind.style.display = isBound ? "none" : "";
  unbind.style.display = isBound ? "" : "none";
  syncBindButton();

  const bar = $("#beanBindingBar");
  if (isBound && binding) {
    const bits = [
      binding.serial ? `${t("序列号", "Serial")} ${esc(binding.serial)}` : "",
      binding.mac ? `${t("MAC", "MAC")} ${esc(binding.mac)}` : "",
      binding.boundAt ? `${t("绑定于", "Bound at")} ${esc(binding.boundAt)}` : "",
    ].filter(Boolean);
    bar.innerHTML = `<div style="border-top:1px solid rgba(31,75,255,.1);padding-top:12px">
      <b style="color:var(--blue)">${t("已绑定录音豆", "Bound recorder")}：${esc(boundName())}</b>
      <div style="${muted};margin-top:6px">${bits.join(" · ") || t("只能连接这台设备", "Only this device can be connected")}</div>
    </div>`;
    $("#beanDevices").innerHTML = `<div style="${cardStyle};border-color:rgba(31,75,255,.35)">
      <b>${t("固定设备", "Fixed device")}</b>
      <div style="${muted};margin-top:6px">${esc(boundName())} · ${t("如需更换，请先解绑", "Unbind first to switch devices")}</div>
    </div>`;
  } else {
    bar.innerHTML = "";
  }
}

async function loadStatus(): Promise<void> {
  const box = $("#beanStatus");
  // 状态接口带出绑定关系；加超时兜底，避免任何网络异常把界面永久卡在"正在检查…"。
  try {
    const s = await Promise.race([
      api<{ available?: boolean; product?: string; transport?: string; features?: string[]; binding?: BeanBinding }>("/api/bean/status"),
      new Promise<never>((_, reject) => window.setTimeout(() => reject(new Error(t("接口未响应", "Status API timed out"))), 5000)),
    ]);
    binding = s.binding?.bound ? s.binding : null;
    if (!s.available) {
      box.textContent = t("当前版本不支持录音豆", "Recorder bean is unavailable in this build");
    } else if (binding) {
      box.textContent = `${t("已绑定", "Bound")}：${boundName()} · ${t("可直接读取录音，解绑后可换设备", "Read recordings now; unbind to switch")}`;
    } else {
      box.textContent = `${s.product ?? "soundcore Work / D3200"} · ${t("扫描后选择设备并点击绑定", "Scan, select a device, then bind it")}`;
    }
    applyBindingUI();
    if (binding) {
      selected = binding.target ?? "";
      void loadFiles();
    }
  } catch (e) {
    box.textContent = `${errorMessage(e)} · ${t("可直接点「扫描设备」尝试", "You can still press Scan devices")}`;
    applyBindingUI();
  }
}

async function scanDevices(): Promise<void> {
  if (binding) {
    toast(t("已绑定录音豆，请先解绑再扫描其他设备", "Recorder bound — unbind before scanning for another"), true);
    return;
  }
  const btn = $<HTMLButtonElement>("#beanScanBtn");
  btn.disabled = true;
  btn.textContent = t("扫描中…", "Scanning…");
  try {
    const r = await withBeanTimeout(
      api<{ devices?: BeanDevice[] }>("/api/bean/scan", { json: {} }),
      t("扫描", "Scanning"),
    );
    // 同一台设备可能在多轮广播里出现多次（RPA 轮换 / 厂商数据缺失），按唯一标识去重。
    const seen = new Set<string>();
    devices = (r.devices ?? [])
      .filter((d) => {
        const key = deviceTarget(d);
        if (!key || seen.has(key)) return false;
        seen.add(key);
        return true;
      })
      .map((d) => ({ ...d }));
    selected = "";
    renderDevices();
    toast(t(`发现 ${devices.length} 台设备，正在读取序列号…`, `Found ${devices.length} device(s), reading serials…`));
    void probeSerials();
  } catch (e) {
    toast(errorMessage(e), true);
    $("#beanDevices").innerHTML = `<div style="${cardStyle};color:#d4483c">${esc(errorMessage(e))}</div>`;
  } finally {
    btn.disabled = false;
    btn.textContent = t("扫描设备", "Scan devices");
  }
}

/** 逐台读序列号：BLE 全局串行，并发会 GATT 冲突；广播包里没有序列号，只能连上读。 */
async function probeSerials(): Promise<void> {
  const list = devices;
  probeActive = true;
  probeCancel = false;
  try {
    for (const d of list) {
      // 已绑定 / 重新扫描 / 用户点了绑定（取消剩余探测）都要立刻退出。
      if (binding || devices !== list || probeCancel) break;
      d.probing = true;
      renderDevices();
      try {
        const r = await withBeanTimeout(
          api<{ serial?: string; battery?: number; firmwareVersion?: string }>("/api/bean/probe", {
            json: { target: deviceTarget(d) },
          }),
          t("读取序列号", "Reading serial"),
        );
        d.serial = r.serial ?? "";
        if (r.battery !== undefined) d.battery = r.battery;
        d.probeError = d.serial ? undefined : t("该设备未回报序列号", "This device reported no serial");
      } catch (e) {
        d.probeError = errorMessage(e);
      } finally {
        d.probing = false;
        renderDevices();
      }
    }
  } finally {
    probeActive = false;
    probeCancel = false;
    for (const wake of probeIdleWaiters.splice(0)) wake();
    renderDevices();
  }
}

/** 序列号展示：已读到就高亮，读不到显示原因，读取中显示进度。 */
function serialLine(d: BeanDevice): string {
  if (d.probing) return `<span style="color:var(--blue)">${t("序列号读取中…", "Reading serial…")}</span>`;
  if (d.serial) return `${t("序列号", "Serial")} <b style="font-family:ui-monospace,monospace;color:#12193c">${esc(d.serial.toUpperCase())}</b>`;
  if (d.probeError) return `<span style="color:#d4483c">${esc(d.probeError)}</span>`;
  return t("序列号未读取", "Serial not read");
}

function renderDevices(): void {
  const box = $("#beanDevices");
  if (binding) {
    applyBindingUI();
    return;
  }
  if (!devices.length) {
    box.innerHTML = `<div style="${cardStyle}">${t("没发现录音豆。确认设备已开机、在蓝牙范围内，并且没有被手机 App 连接。", "No recorder bean found. Make sure it is powered on, nearby, and not connected to a phone app.")}</div>`;
    syncBindButton();
    return;
  }
  box.innerHTML = `
    <div style="font-weight:600;margin:6px 0 10px">${t("附近的设备（序列号是设备唯一标识，选中后点「绑定此设备」）", "Nearby devices (serial identifies the device — select one, then press Bind)")}</div>
    ${devices
      .map((d, i) => {
        const target = deviceTarget(d);
        const active = selected === target;
        return `<button type="button" data-i="${i}" style="${cardStyle};width:100%;text-align:left;cursor:pointer;${
          active ? "border-color:var(--blue);box-shadow:0 0 0 2px rgba(31,75,255,.14)" : ""
        }">
          <div style="display:flex;align-items:center;gap:10px">
            <b style="flex:1">${esc(deviceLabel(d))}</b>
            <span style="${muted}">${esc(d.rssi !== undefined ? `${d.rssi} dBm` : "")}</span>
          </div>
          <div style="margin-top:6px;font-size:12px">${serialLine(d)}</div>
          <div style="${muted};margin-top:4px">${esc([d.productCode, d.macAddress].filter(Boolean).join(" · "))}</div>
        </button>`;
      })
      .join("")}`;
  syncBindButton();
  box.querySelectorAll<HTMLButtonElement>("button[data-i]").forEach((el) => {
    el.onclick = () => {
      selected = deviceTarget(devices[Number(el.dataset.i)]);
      renderDevices();
      // 绑定前绝不预读设备信息/录音列表：那会发起 BLE 连接，和紧接着的「绑定」
      // 并发打同一台设备，WinRT GATT 冲突会让两边一起挂起（按钮卡在"绑定中"）。
      $("#beanDetail").innerHTML = `<div style="${cardStyle}">${t(
        "已选中设备，点「绑定此设备」后会自动读取设备信息和录音列表。",
        "Device selected. Press Bind to load its info and recordings automatically.",
      )}</div>`;
    };
  });
}

async function bindDevice(): Promise<void> {
  const target = selected;
  if (!target) {
    toast(t("请先扫描并选择一台录音豆", "Scan and select a recorder first"), true);
    return;
  }
  const d = devices.find((x) => deviceTarget(x) === target);
  const btn = $<HTMLButtonElement>("#beanBindBtn");
  btn.disabled = true;
  btn.textContent = t("绑定中…", "Binding…");
  try {
    // 若后台仍在读其他设备的序列号，先取消剩余探测并等当前 BLE 会话结束，
    // 否则两路 GATT 连接并发会让绑定请求挂在 WinRT 层（表现为点了没反应）。
    if (probeActive) {
      probeCancel = true;
      await waitProbeIdle();
    }
    const r = await withBeanTimeout(
      api<{ binding?: BeanBinding }>("/api/bean/bind", {
        json: {
          target,
          name: d?.name || d?.advName || "",
          mac: d?.macAddress || "",
          // probe 已读到就走免重连绑定；避免 WinRT 二次连接挂起导致「点了没反应」。
          serial: d?.serial || "",
          expectSerial: d?.serial || "",
        },
      }),
      t("绑定", "Binding"),
    );
    binding = r.binding ?? null;
    applyBindingUI();
    toast(t("绑定成功，之后只会连接这台录音豆", "Bound — only this recorder will be used"));
    await loadStatus();
  } catch (e) {
    toast(errorMessage(e), true);
    syncBindButton();
  }
}

async function unbindDevice(): Promise<void> {
  const btn = $<HTMLButtonElement>("#beanUnbindBtn");
  const old = btn.textContent;
  btn.disabled = true;
  btn.textContent = t("解绑中…", "Unbinding…");
  try {
    await withBeanTimeout(api("/api/bean/unbind", { json: {} }), t("解绑", "Unbinding"));
    binding = null;
    selected = "";
    devices = [];
    autoSyncDone = false;
    $("#beanDetail").innerHTML = "";
    toast(t("已解绑，现在可以绑定其他录音豆", "Unbound — you can bind another recorder now"));
    await loadStatus();
  } catch (e) {
    toast(errorMessage(e), true);
  } finally {
    btn.disabled = false;
    btn.textContent = old ?? t("解绑", "Unbind");
  }
}

async function loadFiles(): Promise<void> {
  const box = $("#beanDetail");
  box.innerHTML = `<div style="${cardStyle}">${t("正在读取设备信息和录音列表…", "Reading device info and recording list…")}</div>`;
  try {
    // 一次连接同时读设备信息和录音清单。旧版分两次请求、两次 GATT 连接，
    // WinRT 对刚断开的设备二次连接常挂起，才会报「读取录音列表耗时过长」。
    const snap = await withBeanTimeout(
      api<{ device?: Record<string, unknown>; files?: { files?: BeanFile[]; fileCount?: number } }>(
        "/api/bean/snapshot",
        { json: { target: selected } },
      ),
      t("读取设备与录音列表", "Reading device and recordings"),
    );
    const dev = snap.device ?? {};
    const files = snap.files?.files ?? [];
    const synced = new Set(readSyncedIds(syncStorageKey()));
    const bits = [
      dev.battery !== undefined ? `${t("电量", "Battery")} ${esc(dev.battery)}%` : "",
      dev.firmwareVersion ? `${t("固件", "Firmware")} ${esc(dev.firmwareVersion)}` : "",
      dev.serialNumber ? `${t("序列号", "Serial")} ${esc(dev.serialNumber)}` : "",
      dev.recording === true ? t("正在录音", "Recording now") : "",
    ].filter(Boolean);
    const infoHtml = `<div style="${cardStyle}">
      <b>${t("设备信息", "Device info")}</b>
      <div style="${muted};margin-top:6px">${bits.join(" · ") || t("无详细信息", "No details")}</div>
    </div>`;
    const syncBar = files.length
      ? `<div id="beanSyncStatus" style="${cardStyle};border-color:rgba(31,75,255,.3);color:var(--blue)">${t("准备自动同步…", "Preparing auto-sync…")}</div>
         <button class="btn" id="beanResyncBtn" style="margin-bottom:10px">${t("重新同步全部", "Resync all")}</button>`
      : "";
    const listHtml = files.length
      ? files
          .map((f) => {
            const when = f.endTime ? new Date(f.endTime * 1000).toLocaleString() : "";
            const done = synced.has(f.fileId);
            return `<div style="${cardStyle};display:flex;align-items:center;gap:12px;flex-wrap:wrap">
              <span class="ico" style="color:var(--blue)">${ICON.file}</span>
              <div style="flex:1;min-width:160px">
                <b>${esc(f.title || t("录音", "Recording"))}</b>
                <div style="${muted}">${esc([when, f.durationMs ? fmt(f.durationMs) : "", f.payloadBytes ? `${(f.payloadBytes / 1024 / 1024).toFixed(1)} MB` : ""].filter(Boolean).join(" · "))}</div>
              </div>
              <button class="btn ${done ? "" : "primary"}" data-file="${f.fileId}" ${done ? "disabled" : ""}>${done ? t("已导入", "Imported") : t("导入", "Import")}</button>
            </div>`;
          })
          .join("")
      : `<div style="${cardStyle}">${t("设备上没有可导入的录音", "No recordings on the device")}</div>`;
    box.innerHTML = infoHtml + syncBar + `<div style="font-weight:600;margin:6px 0 10px">${t("设备里的录音", "Recordings on device")}</div>` + listHtml;
    box.querySelectorAll<HTMLButtonElement>("button[data-file]:not([disabled])").forEach((el) => {
      el.onclick = () => void importFile(Number(el.dataset.file), el);
    });
    const resync = document.getElementById("beanResyncBtn");
    if (resync) resync.onclick = () => void autoSyncFiles(files, true);
    // 绑定后自动把设备里的录音导入 tingo；已导入的按序列号去重，不会重复拉流。
    void autoSyncFiles(files);
  } catch (e) {
    box.innerHTML = `<div style="${cardStyle};color:#d4483c">${esc(errorMessage(e))}</div>`;
  }
}

/** 串行自动导入设备录音：BLE 全局锁决定不能并发；每成功一个就记下 fileId 防重复。 */
async function autoSyncFiles(files: BeanFile[], force = false): Promise<void> {
  if (autoSyncRunning || (!force && autoSyncDone) || !files.length) return;
  const key = syncStorageKey();
  const synced = new Set(force ? [] : readSyncedIds(key));
  const pending = files.filter((f) => !synced.has(f.fileId));
  if (!pending.length) {
    autoSyncDone = true;
    setSyncStatus(t("设备录音已全部同步", "All recordings on the device are synced"));
    return;
  }
  autoSyncRunning = true;
  setSyncStatus(t(`正在同步 ${pending.length} 个录音，请勿关闭窗口…`, `Syncing ${pending.length} recording(s) — keep this window open…`));
  let okCount = 0;
  for (const f of pending) {
    const btn = document.querySelector<HTMLButtonElement>(`button[data-file="${f.fileId}"]`);
    const ok = await importFile(f.fileId, btn, false);
    if (ok) {
      okCount += 1;
      synced.add(f.fileId);
      writeSyncedIds(key, [...synced]);
      if (btn) {
        btn.disabled = true;
        btn.textContent = t("已导入", "Imported");
        btn.classList.remove("primary");
      }
    }
  }
  autoSyncRunning = false;
  autoSyncDone = true;
  setSyncStatus(
    okCount === pending.length
      ? t(`已自动同步 ${okCount} 个录音，转写和纪要在后台进行`, `Synced ${okCount} recording(s) — transcription and minutes run in the background`)
      : t(`已同步 ${okCount} / ${pending.length} 个录音，失败的可点「导入」重试`, `Synced ${okCount} / ${pending.length}; retry failed ones with Import`),
  );
}

async function importFile(fileId: number, btn: HTMLButtonElement | null, navigate = true): Promise<boolean> {
  const old = btn?.textContent ?? null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = t("导入中…", "Importing…");
  }
  try {
    const r = await withBeanTimeout(
      api<{ job?: { meeting?: string } }>("/api/bean/import", {
        json: { target: selected, fileId, deleteAfterImport: false },
      }),
      t("导入", "Importing"),
    );
    toast(t("已开始导入，转写和纪要在后台进行", "Import started — transcription and minutes run in the background"));
    // 批量自动同步时不能跳转到单条会议页，否则后续文件还没导完就离开了本页。
    if (navigate) {
      if (r.job?.meeting) {
        location.hash = `#/m/${encodeURIComponent(r.job.meeting)}`;
      } else {
        location.hash = "#/";
      }
    }
    return true;
  } catch (e) {
    toast(errorMessage(e), true);
    if (btn) {
      btn.disabled = false;
      btn.textContent = old ?? t("导入", "Import");
    }
    return false;
  }
}

async function selfTest(): Promise<void> {
  const box = $("#beanReport");
  box.innerHTML = `<div style="${cardStyle}">${t("正在自检协议、加解密与音频封装…", "Checking protocol, crypto and audio packaging…")}</div>`;
  try {
    const r = await api<Record<string, unknown>>("/api/bean/selftest", { json: {} });
    const ok = r.ok !== false && r.passed !== false;
    box.innerHTML = `<div style="${cardStyle};border-color:${ok ? "rgba(18,179,168,.45)" : "rgba(212,72,60,.45)"}">
      <b style="color:${ok ? "#0f9c92" : "#d4483c"}">${ok ? t("自检通过", "Self-test passed") : t("自检发现问题", "Self-test found problems")}</b>
      <pre style="white-space:pre-wrap;word-break:break-all;font-size:11px;max-height:240px;overflow:auto;margin-top:10px">${esc(JSON.stringify(r, null, 2))}</pre>
    </div>`;
  } catch (e) {
    box.innerHTML = `<div style="${cardStyle};color:#d4483c">${esc(errorMessage(e))}</div>`;
  }
}
