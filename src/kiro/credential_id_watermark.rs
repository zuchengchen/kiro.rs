//! 凭据 ID 高水位（跨重启单调）
//!
//! trace、usage_log、credit_total 都按 credential_id 聚合历史。ID 一旦被新账号复用，
//! 新账号就会继承前任的积分、调用数和失败记录。原来的 `next_id` 在每次启动时按
//! 「现存最大 ID + 1」重算，只在单个进程内单调：删掉最大 ID 的账号再重启，这个 ID
//! 就会被分配给下一个新账号。
//!
//! 这里把「发出过的最大 ID」持久化到凭据文件旁的一个小文件里，启动时 `next_id` 取
//! 「现存最大 ID」与高水位两者的较大值 + 1。
//!
//! 单独成文件而不是写进 credentials.json：多凭据格式是纯数组，放不下额外字段，
//! 改结构会让旧版本二进制和上游都读不了。文件名由凭据文件名派生，不同凭据文件
//! （测试里的多份临时文件）各有独立的 ID 空间。

use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 串行化「读旧值 → 比较 → 写盘」，保证并发写入时高水位只增不减
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatermarkFile {
    max_issued_id: u64,
}

/// 高水位文件路径：`<dir>/<凭据文件名去扩展名>.id_watermark.json`
pub(crate) fn path_for(credentials_path: &Path) -> PathBuf {
    let stem = credentials_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "credentials".to_string());
    credentials_path.with_file_name(format!("{stem}.id_watermark.json"))
}

/// 读取高水位；文件缺失或损坏时返回 0
///
/// 返回 0 是安全的：调用方还会与现存凭据 ID、历史记录中的最大 ID 取较大值，
/// 最坏情况退化为修复前的行为。
pub(crate) fn load(credentials_path: &Path) -> u64 {
    std::fs::read_to_string(path_for(credentials_path))
        .ok()
        .and_then(|content| serde_json::from_str::<WatermarkFile>(&content).ok())
        .map(|file| file.max_issued_id)
        .unwrap_or(0)
}

/// 把高水位推进到至少 `issued_id`；已记录的值更大时不写盘
///
/// 先写临时文件再 rename，进程在写一半时被杀也不会留下半截文件。
pub(crate) fn store(credentials_path: &Path, issued_id: u64) -> anyhow::Result<()> {
    use anyhow::Context;

    let _guard = WRITE_LOCK.lock();
    if load(credentials_path) >= issued_id {
        return Ok(());
    }
    let path = path_for(credentials_path);
    let json = serde_json::to_string_pretty(&WatermarkFile {
        max_issued_id: issued_id,
    })
    .context("序列化凭据 ID 高水位失败")?;
    let tmp = path.with_extension("json.tmp");
    // 直接同步写，不用 block_in_place：文件只有几十字节，只在启动和添加账号时写；
    // block_in_place 在 current_thread 运行时里会 panic，而 new() 可能在那种运行时里被调用。
    let result = std::fs::write(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("写入凭据 ID 高水位失败: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_credentials_path(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kiro_id_watermark_{}_{}_{}",
            name,
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("credentials.json")
    }

    #[test]
    fn path_is_derived_from_credentials_file_name() {
        assert_eq!(
            path_for(Path::new("/data/credentials.json")),
            PathBuf::from("/data/credentials.id_watermark.json")
        );
        assert_eq!(
            path_for(Path::new("/tmp/kiro_test_a.json")),
            PathBuf::from("/tmp/kiro_test_a.id_watermark.json")
        );
    }

    #[test]
    fn missing_or_corrupt_file_reads_as_zero() {
        let creds = tmp_credentials_path("corrupt");
        assert_eq!(load(&creds), 0);
        std::fs::write(path_for(&creds), "{not json").unwrap();
        assert_eq!(load(&creds), 0);
        let _ = std::fs::remove_dir_all(creds.parent().unwrap());
    }

    #[test]
    fn store_only_moves_forward() {
        let creds = tmp_credentials_path("monotonic");
        store(&creds, 5).unwrap();
        assert_eq!(load(&creds), 5);
        store(&creds, 3).unwrap();
        assert_eq!(load(&creds), 5, "较小的值不能覆盖已记录的高水位");
        store(&creds, 9).unwrap();
        assert_eq!(load(&creds), 9);
        assert!(
            !path_for(&creds).with_extension("json.tmp").exists(),
            "rename 后不应残留临时文件"
        );
        let _ = std::fs::remove_dir_all(creds.parent().unwrap());
    }
}
