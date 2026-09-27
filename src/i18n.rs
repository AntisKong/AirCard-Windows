use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    English,
    SimplifiedChinese,
}

impl Default for Language {
    fn default() -> Self {
        Self::English
    }
}

impl Language {
    pub fn load() -> Self {
        let path = settings_path();
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(settings) = serde_json::from_str::<Settings>(&contents) {
                return settings.language;
            }
        }

        let system_locale = std::env::var("LANG")
            .or_else(|_| std::env::var("LANGUAGE"))
            .or_else(|_| std::env::var("LC_ALL"))
            .unwrap_or_default()
            .to_ascii_lowercase();

        if system_locale.starts_with("zh") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    pub fn save(self) {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let settings = Settings { language: self };
        if let Ok(contents) = serde_json::to_string_pretty(&settings) {
            let _ = fs::write(path, contents);
        }
    }

    pub fn text<'a>(self, source: &'a str) -> &'a str {
        if self == Self::English {
            return source;
        }

        match source {
            "Wallet" => "钱包",
            "Wallet artwork studio" => "钱包卡面工作台",
            "Your card" => "你的卡片",
            "Choose a card and its new artwork" => "选择卡片和新卡面",
            "Artwork" => "卡面图片",
            "Preview" => "预览",
            "Refresh" => "刷新",
            "Auto (USB preferred)" => "自动（优先 USB）",
            "USB only" => "仅 USB",
            "WiFi only" => "仅 WiFi",
            "No device" => "没有设备",
            "Archived iPhone" => "离线 iPhone",
            "Ready" => "就绪",
            "Unavailable" => "不可用",
            "Logs" => "日志",
            "Logs [x]" => "日志 [x]",
            "Copy Logs" => "复制日志",
            "Save to File..." => "保存到文件...",
            "Clear" => "清空",
            "No events logged yet." => "暂无日志记录。",
            "Language" => "语言",
            "English" => "英语",
            "Simplified Chinese" => "简体中文",
            "Language changed." => "语言已切换。",
            "Transport mode" => "连接方式",
            "entries" => "条记录",
            "Ready. Connect iPhone via USB or paired WiFi and unlock it." => "就绪。请通过 USB 或已配对的 WiFi 连接并解锁 iPhone。",
            "No iPhone connected via USB or paired WiFi." => "未检测到通过 USB 或已配对 WiFi 连接的 iPhone。",
            "Please select a connected iPhone." => "请选择已连接的 iPhone。",
            "Disconnect the USB cable and refresh to guarantee the full AirTraffic path uses WiFi." => "请断开 USB 线并刷新，以确保完整的 AirTraffic 路径使用 WiFi。",
            "Please enter or scan a target card hash." => "请输入或扫描目标卡片 Hash。",
            "Please choose a card skin image first." => "请先选择卡片皮肤图片。",
            "Crop position updated." => "裁切位置已更新。",
            "Scanning syslog... Open Wallet or tap your card on iPhone." => "正在扫描 syslog... 请在 iPhone 上打开钱包并点击卡片。",
            "Syslog scanning stopped." => "syslog 扫描已停止。",
            "Writing card skin to iPhone..." => "正在将卡片皮肤写入 iPhone...",
            "Syslog scan finished" => "syslog 扫描完成",
            "Card skin successfully flashed! Force quit Wallet on iPhone and reopen it." => "卡片皮肤应用成功！请在 iPhone 上强制关闭并重新打开 Wallet。",
            "Card skin written without original backup; restore is unavailable. Force quit Wallet and reopen it." => "卡面已写入，但无法读取原卡面备份，因此不能恢复原卡面。请强制关闭并重新打开 Wallet。",
            "Card skin updated successfully!" => "卡片皮肤更新成功！",
            "Restoring original card face..." => "正在恢复原卡面...",
            "Restoring original card artwork..." => "正在恢复原卡面图片...",
            "Clearing .cache cache..." => "正在清理 .cache 缓存...",
            "Clearing .pkcache cache..." => "正在清理 .pkcache 缓存...",
            "Original card face restored successfully!" => "原卡面恢复成功！",
            "Original card face restored. Force close Wallet and reopen it." => "原卡面已恢复。请强制关闭并重新打开 Wallet。",
            "Original card backup not found." => "未找到原卡面备份。",
            "Original artwork was not readable; restore is unavailable." => "无法读取原卡面，不能使用恢复功能。",
            "Apply a card skin once to create an original backup." => "首次写入前需成功读取原卡面，才可建立可恢复记录。",
            "Card captured" => "已捕获卡片",
            "Error: " => "错误：",
            "Found" => "已找到",
            "connected device(s); transport mode:" => "台已连接设备；连接方式：",
            "Could not enumerate devices:" => "无法枚举设备：",
            "Selected iPhone has no" => "选中的 iPhone 没有",
            "connection. Refresh devices or change transport mode." => "连接。请刷新设备或更改连接方式。",
            "Scanning syslog..." => "正在扫描 syslog...",
            "Open Wallet on iPhone and tap your card" => "请在 iPhone 上打开钱包并点击目标卡片",
            "Stop" => "停止",
            "Scan" => "扫描",
            "Target Card Hash" => "目标卡片 Hash",
            "Base64 pass hash..." => "Base64 卡片 Hash...",
            "Saved cards" => "已保存的卡片",
            "Changed cards on this iPhone" => "这台 iPhone 已修改的卡片",
            "Art history" => "卡面记录",
            "01  Original · saved before first write" => "01  原始卡面 · 首次写入前保存",
            "01  Original unavailable · restore disabled" => "01  原始卡面不可读取 · 无法恢复",
            "02  Changed · latest successful write" => "02  修改卡面 · 最近一次成功写入",
            "01 / ORIGINAL" => "01 / 原始",
            "02 / CHANGED" => "02 / 修改后",
            "Preview unavailable" => "无法预览",
            "Select..." => "请选择...",
            "PNG, JPG, WebP - auto-scaled to 1536x969" => "PNG、JPG、WebP，将自动缩放到 1536x969",
            "Drag inside the preview to reposition the crop." => "在预览区域内拖动以调整裁切位置。",
            "Choose Image..." => "选择图片...",
            "Export PNG" => "导出 PNG",
            "Write to iPhone" => "写入 iPhone",
            "Apply Card Skin" => "应用卡片皮肤",
            "Restore Original" => "恢复原卡面",
            "connect iPhone" => "连接 iPhone",
            "choose available transport" => "选择可用的连接方式",
            "enter card hash" => "输入卡片 Hash",
            "choose image" => "选择图片",
            "Need: " => "需要：",
            "1536 x 969 px pass canvas" => "1536 x 969 像素卡片画布",
            "No artwork loaded" => "尚未加载图片",
            "No image" => "没有图片",
            "After applying, force close Apple Wallet and reopen it." => "应用后，请强制关闭 Apple Wallet 并重新打开。",
            _ => source,
        }
    }

    pub fn option_label(self, option: Self) -> &'static str {
        match (self, option) {
            (Self::English, Self::English) => "English",
            (Self::English, Self::SimplifiedChinese) => "Simplified Chinese",
            (Self::SimplifiedChinese, Self::English) => "英语",
            (Self::SimplifiedChinese, Self::SimplifiedChinese) => "简体中文",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Settings {
    language: Language,
}

fn settings_path() -> PathBuf {
    crate::portable_data::directory().join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_translation_has_english_fallback() {
        assert_eq!(Language::SimplifiedChinese.text("Wallet"), "钱包");
        assert_eq!(
            Language::SimplifiedChinese.text("unknown string"),
            "unknown string"
        );
    }

    #[test]
    fn language_option_labels_follow_current_language() {
        assert_eq!(
            Language::English.option_label(Language::SimplifiedChinese),
            "Simplified Chinese"
        );
        assert_eq!(
            Language::SimplifiedChinese.option_label(Language::SimplifiedChinese),
            "简体中文"
        );
    }
}
