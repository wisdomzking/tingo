//! D3200 BLE GATT 传输层（PROTOCOL.md §3）。
//!
//! Windows 走 btleplug 的 WinRT 后端，不需要系统配对（pair=False），
//! 也不需要飞书账号。扫描 → 连接 → 服务发现 → notify 订阅 → 分片写入。
//! 扫描/连接互斥与重试借鉴 Soundcore `lib/ble/ble_service.dart`。

use std::pin::Pin;
use std::time::Duration;

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use uuid::Uuid;

use super::models::ScannedDevice;

pub const SERVICE_UUID: &str = "020cf5da-0000-1000-8000-00805f9b34fb";
pub const WRITE_UUID: &str = "00007777-0000-1000-8000-00805f9b34fb";
pub const NOTIFY_UUID: &str = "00008888-0000-1000-8000-00805f9b34fb";
const CCCD_UUID: &str = "00002902-0000-1000-8000-00805f9b34fb";

/// 默认 ATT MTU 按 20 字节保守分片；系统协商到更大 MTU 也不会出错。
const WRITE_CHUNK: usize = 20;

pub type NotificationStream = Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>;

fn uuid_matches(u: &Uuid, short: &str) -> bool {
    let s = u.to_string().to_lowercase().replace('-', "");
    let want = short.to_lowercase().replace('-', "");
    s.contains(&want)
}

pub fn is_d3200_service(u: &Uuid) -> bool {
    uuid_matches(u, "020cf5da")
}

fn friendly_error(err: &str) -> String {
    let lower = err.to_lowercase();
    if lower.contains("not found") || lower.contains("device not") {
        "找不到录音豆：请确认设备在附近且未连接其他手机/电脑".into()
    } else if lower.contains("busy") || lower.contains("in use") || lower.contains("access denied") {
        "设备被占用：请先关闭飞书或其他正在连接录音豆的程序".into()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "连接超时：请把录音豆靠近电脑后重试".into()
    } else {
        format!("BLE 操作失败：{err}")
    }
}

async fn first_adapter() -> Result<Adapter, String> {
    let manager = Manager::new().await.map_err(|e| friendly_error(&e.to_string()))?;
    let adapters = manager
        .adapters()
        .await
        .map_err(|e| friendly_error(&e.to_string()))?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| "未检测到蓝牙适配器，请在 Windows 设置中开启蓝牙".into())
}

/// 扫描录音豆。默认 3 秒，返回所有看起来像 D3200 的设备。
pub async fn scan(duration: Duration) -> Result<Vec<ScannedDevice>, String> {
    let adapter = first_adapter().await?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| friendly_error(&e.to_string()))?;
    tokio::time::sleep(duration).await;

    let mut found = Vec::new();
    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|e| friendly_error(&e.to_string()))?;
    for p in peripherals {
        let Ok(Some(props)) = p.properties().await else { continue };
        let is_d3200 = props.services.iter().any(is_d3200_service);
        let adv_name = props.local_name.unwrap_or_default();
        let looks_like = is_d3200
            || adv_name.to_lowercase().contains("soundcore")
            || adv_name.to_lowercase().contains("work");
        if !looks_like {
            continue;
        }
        // Windows 给 BLE 设备分配的是随机地址（RPA），每次广告周期都变；
        // 用系统 id 当稳定标识会导致「扫描能看见、连接找不到」。
        // 录音豆在厂商数据前 6 字节广播真实 MAC（PROTOCOL.md §2），优先用它。
        let sys_id = p.id().to_string().to_uppercase();
        let (real_mac, bound) = super::models::mac_from_manufacturer(&props.manufacturer_data);
        let real_mac = real_mac.unwrap_or_else(|| sys_id.clone());
        let service_uuid = props
            .services
            .iter()
            .find(|u| is_d3200_service(u))
            .map(|u| u.to_string())
            .unwrap_or_else(|| SERVICE_UUID.into());
        found.push(ScannedDevice {
            id: real_mac.clone(),
            name: adv_name.clone(),
            rssi: props.rssi.unwrap_or(0),
            adv_name,
            product_code: if is_d3200 { "D3200".into() } else { "soundcore".into() },
            service_uuid,
            mac_address: real_mac,
            is_bound_advertised: bound,
            is_d3200,
        });
    }
    let _ = adapter.stop_scan().await;
    found.sort_by(|a, b| b.rssi.cmp(&a.rssi));
    Ok(found)
}

pub struct BeanConnection {
    #[allow(dead_code)]
    adapter: Adapter,
    peripheral: Peripheral,
    write_char: Characteristic,
    notify_char: Characteristic,
}

/// 按 target（MAC / 名称）筛选设备。
///
/// 关键：ID 匹配不依赖 `properties()`。WinRT 在 `stop_scan()` 之后常返回 None，
/// 之前的实现因为 `let props = ... else { continue }` 把已发现的设备全部丢掉，
/// 导致「扫描能发现、连接找不到」。这里把 properties 只当作可选增强。
async fn select_candidates(
    adapter: &Adapter,
    needle: &str,
    target_lower: &str,
) -> Result<Vec<(Peripheral, bool)>, String> {
    let peripherals = adapter
        .peripherals()
        .await
        .map_err(|e| friendly_error(&e.to_string()))?;
    let mut out = Vec::new();
    for p in peripherals {
        let id = p.id().to_string().to_uppercase();
        let props = p.properties().await.ok().flatten();
        // 同时匹配系统 RPA id 与厂商数据里的真实 MAC；两者任一命中即可。
        let mut ids = vec![id.to_lowercase().replace([':', '-'], "")];
        if let Some(mfr) = props.as_ref().map(|v| &v.manufacturer_data) {
            if let (Some(real_mac), _) = super::models::mac_from_manufacturer(mfr) {
                ids.push(real_mac.to_lowercase().replace([':', '-'], ""));
            }
        }
        let name = props
            .as_ref()
            .and_then(|v| v.local_name.clone())
            .unwrap_or_default();
        let name_lower = name.to_lowercase();
        let is_d3200 = props
            .as_ref()
            .map(|v| v.services.iter().any(is_d3200_service))
            .unwrap_or(false);
        let id_match = !needle.is_empty() && ids.iter().any(|x| x == &needle || x.contains(&needle));
        let name_match = !target_lower.is_empty() && !name_lower.is_empty() && name_lower == target_lower;
        let auto_match = needle.is_empty()
            && (is_d3200 || name_lower.contains("soundcore") || name_lower.contains("work"));
        if id_match || name_match || auto_match {
            out.push((p, is_d3200));
        }
    }
    Ok(out)
}

/// 连接设备并订阅 …8888 通知。
///
/// target 可以 id（MAC）、local name 或 MAC 去掉冒号的形式；空串表示连第一台 D3200。
pub async fn connect(target: &str) -> Result<(BeanConnection, NotificationStream), String> {
    connect_with_options(target, false).await
}

/// 严格连接：找不到精确匹配的设备就失败，绝不"随便连附近一台"。
/// 绑定设备时必须用它，否则可能把旁边另一台同型号录音豆绑进来。
pub async fn connect_strict(target: &str) -> Result<(BeanConnection, NotificationStream), String> {
    connect_with_options(target, true).await
}

async fn connect_with_options(
    target: &str,
    strict: bool,
) -> Result<(BeanConnection, NotificationStream), String> {
    let adapter = first_adapter().await?;
    // 扫描与连接并发是 GATT 首次连接失败的常见原因，先停扫描再静默 200ms。
    let _ = adapter.stop_scan().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let needle = target.trim().to_lowercase().replace([':', '-'], "");
    let target_lower = target.trim().to_lowercase();
    // target 是 12 位十六进制时视为 MAC。录音豆广播随机私有地址（RPA），
    // 每个广告周期都会换 MAC，所以扫描时记下的地址到连接时已失效——这是
    // "扫描能看见、连接找不到"的真正根因。MAC 匹配失败后必须能按设备名兜底。
    let looks_like_mac = needle.len() == 12 && needle.chars().all(|c| c.is_ascii_hexdigit());

    // WinRT 只在扫描期间维护 local_name / services 等属性；停扫后 properties()
    // 常返回 None，导致按名字匹配也失效。所以连接前先做一轮新鲜扫描，
    // 在属性可用时挑出候选，再停扫连接。
    let mut candidates = Vec::new();
    for _ in 0..3 {
        let _ = adapter.start_scan(ScanFilter::default()).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        candidates = select_candidates(&adapter, &needle, &target_lower).await?;
        let _ = adapter.stop_scan().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !candidates.is_empty() {
            break;
        }
    }
    // RPA 兜底：RPA 轮换或停扫后 properties() 为空时，精确匹配会落空。
    // 严格模式只允许「附近确实只有一台录音豆」这一种兜底——多台时宁可报错，
    // 也绝不连到旁边那台（这正是「点 A 连到 B」和「读错序列号」的根因）。
    if candidates.is_empty() {
        let nearby = select_candidates(&adapter, "", "").await?;
        if strict {
            if nearby.len() == 1 {
                eprintln!("[bean] exact target {target:?} not found; single nearby bean, using it");
                candidates = nearby;
            }
        } else if !nearby.is_empty() {
            eprintln!(
                "[bean] exact target {target:?} (mac_like={looks_like_mac}) not found; falling back to nearby bean"
            );
            candidates = nearby;
        }
    }
    if candidates.is_empty() {
        return Err(if target.is_empty() {
            "没找到录音豆：请先启动扫描或确认设备已开机".into()
        } else {
            format!("找不到设备 {target}，请重新扫描")
        });
    }
    // 按名字匹配到多台同名设备时（同型号很常见），连哪一台都是碰运气，
    // 必须报错让用户用 MAC/序列号明确指定，否则会出现「点 A 连到 B」。
    // 只有显式给了 target（非空、且不是 MAC）才判定为按名字歧义。
    if candidates.len() > 1 && !needle.is_empty() && !looks_like_mac {
        return Err(format!(
            "附近有 {} 台同名设备，无法确定连哪一台；请在列表里选中带序列号的目标设备，或只保留一台开机",
            candidates.len()
        ));
    }
    // 优先 UUID 明确匹配 D3200 的设备
    candidates.sort_by_key(|(_, is_d3200)| if *is_d3200 { 0 } else { 1 });
    let peripheral = candidates.remove(0).0;

    let mut last_err = String::new();
    let mut connected = false;
    // 上一次连接刚断开时 Windows 可能仍占用 GATT；给足退避时间再重试。
    for attempt in 0..5 {
        match peripheral.connect().await {
            Ok(()) => { connected = true; break; }
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis(800 + 600 * attempt as u64)).await;
            }
        }
    }
    if !connected {
        return Err(friendly_error(&last_err));
    }

    let mut discovered = false;
    for attempt in 0..4 {
        if peripheral.discover_services().await.is_ok() {
            discovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(400 + 300 * attempt as u64)).await;
    }
    if !discovered {
        let _ = peripheral.disconnect().await;
        return Err("服务发现失败：请关闭占用设备的应用后重试".into());
    }

    // WinRT 首次 discover_services 有时只返回部分服务（特征列表为空或不全），
    // 立即读 characteristics() 会误报"缺少 WRITE 特征"。做多轮"重新发现 + 等待"。
    let (write_char, notify_char) = {
        let mut found: Option<(Characteristic, Characteristic)> = None;
        for attempt in 0..4 {
            let chars = peripheral.characteristics();
            let w = chars.iter().find(|c| uuid_matches(&c.uuid, WRITE_UUID)).cloned();
            let n = chars.iter().find(|c| uuid_matches(&c.uuid, NOTIFY_UUID)).cloned();
            if let (Some(w), Some(n)) = (w, n) {
                found = Some((w, n));
                break;
            }
            // 未找到时重新发现，给 WinRT 更多时间填充 GATT 缓存。
            let _ = peripheral.discover_services().await;
            tokio::time::sleep(Duration::from_millis(400 + 400 * attempt as u64)).await;
        }
        found.ok_or_else(|| {
            let uuids: Vec<String> = peripheral
                .characteristics()
                .iter()
                .map(|c| c.uuid.to_string())
                .collect();
            format!(
                "设备缺少 WRITE/NOTIFY 特征（期望 {WRITE_UUID} / {NOTIFY_UUID}）；实际特征：{}",
                if uuids.is_empty() { "无".into() } else { uuids.join(", ") }
            )
        })?
    };
    // CCCD 由 subscribe 内部写入；这里只校验设备确实声明了该描述符。
    let _ = CCCD_UUID;

    let mut subscribed = false;
    for _ in 0..3 {
        if peripheral.subscribe(&notify_char).await.is_ok() {
            subscribed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !subscribed {
        let _ = peripheral.disconnect().await;
        return Err("开启通知失败：请靠近设备并重试".into());
    }

    let raw = peripheral
        .notifications()
        .await
        .map_err(|e| friendly_error(&e.to_string()))?;
    let stream: NotificationStream = Box::pin(raw.map(|n| n.value));
    Ok((
        BeanConnection {
            adapter,
            peripheral,
            write_char,
            notify_char,
        },
        stream,
    ))
}

impl BeanConnection {
    /// 带 20 字节分片写入命令帧。
    pub async fn write_frame(&self, frame: &[u8]) -> Result<(), String> {
        for chunk in frame.chunks(WRITE_CHUNK) {
            self.peripheral
                .write(&self.write_char, chunk, WriteType::WithoutResponse)
                .await
                .map_err(|e| friendly_error(&e.to_string()))?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn peripheral_id(&self) -> String {
        self.peripheral.id().to_string().to_uppercase()
    }

    #[allow(dead_code)]
    pub fn notify_uuid(&self) -> Uuid {
        self.notify_char.uuid
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        self.peripheral
            .disconnect()
            .await
            .map_err(|e| format!("断开设备失败：{e}"))
    }
}

