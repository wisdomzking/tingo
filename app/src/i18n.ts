/* 界面语言：中文 / English。默认跟系统语言，设置里可以改（存在配置文件 [ui] language）。
   文案就地写成 t("中文", "English")，读代码时两种语言一眼都能看到；
   以后加别的语言，可以按中文原文做键补一张对照表。 */

export type Lang = "zh" | "en";
export type LangPref = "auto" | Lang;

const KEY = "tingo.lang"; // 记住上次的语言，启动时先用它渲染，避免先闪一下中文

const systemLang = (): Lang => (navigator.language || "").toLowerCase().startsWith("zh") ? "zh" : "en";

function initial(): Lang {
  try {
    const v = localStorage.getItem(KEY);
    if (v === "zh" || v === "en") return v;
  } catch {
    /* 无痕模式等拿不到 localStorage */
  }
  return systemLang();
}

let current: Lang = initial();

export const lang = (): Lang => current;
export const isEn = (): boolean => current === "en";
export const t = (zh: string, en: string): string => (current === "en" ? en : zh);

/** 按设置决定语言，返回是否变了 */
export function applyLang(pref: LangPref | undefined): boolean {
  const next = pref === "zh" || pref === "en" ? pref : systemLang();
  try {
    localStorage.setItem(KEY, next);
  } catch {
    /* 同上 */
  }
  const changed = next !== current;
  current = next;
  document.documentElement.lang = next === "zh" ? "zh-CN" : "en";
  return changed;
}

/** 没改过名的说话人（后端存的是"说话人N"），英文界面显示成 Speaker N */
export const speakerName = (name: string): string => {
  const m = /^说话人(\d+)$/.exec(name);
  return m && current === "en" ? `Speaker ${m[1]}` : name;
};

/** 纪要本身是哪种语言（英文纪要有 "## Summary" 这一节）。导出的文件跟纪要语言走，不跟界面语言 */
export const docIsEn = (md: string): boolean => /^##\s+Summary\b/m.test(md);

/** 按指定语言取文案：en 为 undefined 时跟界面语言 */
export const tl = (en: boolean | undefined, zh: string, enText: string): string => (en === undefined ? t(zh, enText) : en ? enText : zh);

/** 日期、数字等的区域设置 */
export const locale = (): string => (current === "en" ? "en-US" : "zh-CN");

/** 页面里写死的文字（侧边栏、拖拽提示）：data-t="中文|English" */
export function renderShell(): void {
  document.querySelectorAll<HTMLElement>("[data-t]").forEach((el) => {
    const [zh, en] = (el.dataset.t ?? "").split("|");
    el.textContent = t(zh, en ?? zh);
  });
}

/** 当前平台：mac / win / other。录音系统与权限提示按它切换 */
export const platform = (): "mac" | "win" | "other" => {
  const uaData = (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData;
  const p = uaData?.platform || navigator.platform;
  if (/Mac/i.test(p)) return "mac";
  if (/Win/i.test(p)) return "win";
  return "other";
};
export const isMac = (): boolean => platform() === "mac";

/** 系统声音不可用时的原因提示（按平台给可操作的指引） */
export const systemAudioHint = (): string =>
  platform() === "win"
    ? t(
        "需要「立体声混音」或 VB-CABLE 回环设备；不影响导入录音",
        "Needs Stereo Mix or a VB-CABLE loopback device; importing recordings still works"
      )
    : t(
        "需要 macOS 15+ 和 Xcode 命令行工具；不影响导入录音",
        "Needs macOS 15+ and Xcode Command Line Tools; importing recordings still works"
      );

/** 不支持设备状态文案（新手引导） */
export const unsupportedHint = (): string =>
  platform() === "win"
    ? t("这台电脑没有可用的回环设备（需立体声混音/VB-CABLE）", "No loopback device on this PC (needs Stereo Mix/VB-CABLE)")
    : t("这台 Mac 不支持（需要 macOS 15+）", "Not supported on this Mac (needs macOS 15+)");
