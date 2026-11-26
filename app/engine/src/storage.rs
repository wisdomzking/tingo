//! Copy a meeting library without deleting or overwriting the original files.
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use serde::Serialize;
use crate::i18n::tr;

/// 纯词法规范化绝对路径（不访问文件系统、不要求存在），用于目录创建前的父子包含校验。
/// 展开 `.` / `..` / 多余分隔符；Windows 盘符统一小写以便比较。
fn normalize_abs(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => {
                // Windows 盘符（如 `D:`）统一成小写，避免 D:/ 与 d:/ 比较不一致
                let raw = p.as_os_str().to_string_lossy().to_lowercase();
                out.push(raw);
            }
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => { out.pop(); }
            Component::Normal(c) => out.push(c),
        }
    }
    out
}

#[derive(Clone, Default, Serialize)]
pub struct Progress {
    pub active: bool,
    pub copied_bytes: u64,
    pub total_bytes: u64,
}

pub fn destination(source: &Path, input: &str) -> Result<PathBuf, String> {
    let path = if let Some(tail) = input.trim().strip_prefix("~/") {
        crate::platform::home_dir().ok_or("HOME unavailable")?.join(tail)
    } else { PathBuf::from(input.trim()) };
    if !path.is_absolute() { return Err(tr("请选择绝对路径的文件夹", "Choose an absolute folder path").into()); }
    let source_canon = crate::platform::clean_path(source.canonicalize().unwrap_or_else(|_| source.to_path_buf()));
    let source_norm = normalize_abs(&source_canon);

    // 目录不存在时自动创建：用户在设置里手填新路径是常见用法，
    // 原先直接 canonicalize 会报 “系统找不到指定的路径 (os error 3)”。
    // 创建前先按规范化后的绝对路径做父子包含校验，避免误建到源目录内部。
    // 两侧都用 normalize_abs（Windows 盘符统一小写），避免 D:/ 与 d:/ 比较不一致。
    let abs = normalize_abs(&path);
    if abs.starts_with(&source_norm) || source_norm.starts_with(&abs) {
        // 相等（选了当前目录）是允许的，交给下面 canonicalize 后统一返回真实路径
        if abs != source_norm {
            return Err(tr("新旧目录不能互相包含，请选择另一个独立文件夹", "The folders must not contain each other").into());
        }
    }
    if !path.exists() {
        fs::create_dir_all(&path).map_err(|e| {
            if crate::i18n::is_en() {
                format!("Cannot create folder \"{}\": {}", path.display(), e)
            } else {
                format!("无法创建目录「{}」：{}", path.display(), e)
            }
        })?;
    }
    let path = crate::platform::clean_path(path.canonicalize().map_err(|e| {
        if crate::i18n::is_en() {
            format!("Cannot access folder \"{}\": {}", path.display(), e)
        } else {
            format!("无法访问目录「{}」：{}", path.display(), e)
        }
    })?);
    if !path.is_dir() { return Err(tr("请选择文件夹", "Choose a folder").into()); }
    if path != source_canon && (path.starts_with(&source_canon) || source_canon.starts_with(&path)) {
        return Err(tr("新旧目录不能互相包含，请选择另一个独立文件夹", "The folders must not contain each other").into());
    }
    let probe = path.join(format!(".tingo-write-check-{}", std::process::id()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&probe).map_err(|e| e.to_string())?;
    let result = file.write_all(b"Tingo").and_then(|_| file.sync_all());
    drop(file);
    let _ = fs::remove_file(probe);
    result.map_err(|e| e.to_string())?;
    Ok(path)
}

fn inventory(dir: &Path, entries: &mut Vec<(PathBuf, bool, u64)>, root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !meta.is_dir() && !meta.is_file() {
            return Err(format!("{}: {}", tr("目录中有链接或特殊文件，无法自动复制", "Cannot copy links or special files"), path.display()));
        }
        entries.push((path.strip_prefix(root).unwrap().to_path_buf(), meta.is_dir(), meta.len()));
        if meta.is_dir() { inventory(&path, entries, root)?; }
    }
    Ok(())
}

/// Stage and verify every file first. Errors never remove the source library.
pub fn copy_library(source: &Path, target: &Path, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    // 源目录尚不存在（还没录过任何会议）时没有可复制的内容，直接成功。
    // 之前会对不存在的 source 调 read_dir 报 “os error 3”，导致空用户无法切换目录。
    if !source.is_dir() {
        progress(0, 0);
        return Ok(());
    }
    if fs::read_dir(target).map_err(|e| e.to_string())?.next().is_some() {
        return Err(tr("复制会议需要一个空文件夹，避免覆盖已有文件", "Choose an empty folder to copy meetings without overwriting files").into());
    }
    let mut entries = Vec::new();
    inventory(source, &mut entries, source)?;
    let total = entries.iter().filter(|(_, d, _)| !d).map(|(_, _, n)| n).sum();
    if crate::meetings::free_bytes(target).is_some_and(|free| free < total) {
        return Err(tr("目标磁盘空间不足", "Not enough space on the destination disk").into());
    }
    progress(0, total);
    let stage = target.join(format!(".tingo-copy-{}", std::process::id()));
    fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let result = (|| {
        let mut copied = 0;
        let mut buffer = vec![0u8; 1024 * 1024];
        for (relative, directory, _) in entries {
            let src = source.join(&relative);
            let dst = stage.join(&relative);
            if directory { fs::create_dir(&dst).map_err(|e| e.to_string())?; continue; }
            let mut input = fs::File::open(&src).map_err(|e| e.to_string())?;
            let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&dst).map_err(|e| e.to_string())?;
            loop {
                let n = input.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 { break; }
                output.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
                copied += n as u64;
                progress(copied, total);
            }
            // Keep history timestamps and the dates of imported meetings.
            output.set_times(fs::FileTimes::new().set_modified(input.metadata().map_err(|e| e.to_string())?.modified().map_err(|e| e.to_string())?)).map_err(|e| e.to_string())?;
            output.sync_all().map_err(|e| e.to_string())?;
            if crate::update::sha256_file(&src).map_err(|e| e.to_string())? != crate::update::sha256_file(&dst).map_err(|e| e.to_string())? {
                return Err(tr("复制校验失败，仍使用原目录", "Copy verification failed; the original folder is still in use").into());
            }
        }
        for entry in fs::read_dir(&stage).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let destination = target.join(entry.file_name());
            if destination.exists() { return Err(tr("目标目录出现同名文件，已停止切换", "A conflicting file appeared; switching stopped").into()); }
            fs::rename(entry.path(), destination).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(stage);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_creates_missing_folder() {
        let base = std::env::temp_dir().join(format!("tingo-storage-dest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        // 目标目录尚不存在——这是用户在设置里手填新路径的常见场景。
        let target = base.join("brand_new_folder");
        assert!(!target.exists());
        let got = destination(&source, target.to_str().unwrap()).unwrap();
        // Windows 下 canonicalize() 会带 \\?\ 前缀；destination() 已统一清理，比较时也清理
        let expected = crate::platform::clean_path(target.canonicalize().unwrap());
        assert_eq!(got, expected);
        assert!(target.is_dir());
        // 二次调用应当也能成功（目录已存在的路径）
        assert!(destination(&source, target.to_str().unwrap()).is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn destination_rejects_nested_folder() {
        let base = std::env::temp_dir().join(format!("tingo-storage-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let source = base.join("source");
        fs::create_dir_all(source.join("sub")).unwrap();
        let nested = source.join("sub");
        // 即使子目录存在，也必须拒绝（新旧目录不能互相包含）
        assert!(destination(&source, nested.to_str().unwrap()).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
