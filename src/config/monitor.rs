//! 本地状态监控配置：只影响采样节奏与保留时长，解析时不读 /proc 也不启动线程。
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 关闭后不启动采样线程，右栏监控页显示停用提示。
    pub enabled: bool,
    /// CPU/内存采样间隔（秒），1–5；下限避免把采样做成忙轮询。
    pub interval_seconds: f64,
    /// 历史保留时长（秒），60–900；换算成定长环形队列的点数。
    pub history_seconds: f64,
    /// 磁盘刷新间隔（秒），5–120；磁盘刷新比 CPU/内存贵一个数量级。
    pub disk_seconds: f64,
    /// 超过这个 CPU 百分比就发通知；0 表示不告警。
    pub alert_cpu: u8,
    /// 内存使用率阈值（百分比），0 表示不告警。
    pub alert_memory: u8,
    /// 磁盘使用率阈值（百分比），0 表示不告警。
    pub alert_disk: u8,
    /// 同一条告警的最短间隔（秒），30–3600；避免在阈值附近来回抖动时刷屏。
    pub alert_seconds: f64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: crate::monitor::DEFAULT_INTERVAL.as_secs_f64(),
            history_seconds: crate::monitor::DEFAULT_HISTORY.as_secs_f64(),
            disk_seconds: crate::monitor::DEFAULT_DISK_INTERVAL.as_secs_f64(),
            // 默认全关：阈值要看机器和负载来定，不由应用替用户猜。
            alert_cpu: 0,
            alert_memory: 0,
            alert_disk: 0,
            alert_seconds: DEFAULT_ALERT_SECONDS,
        }
    }
}

/// 告警最短间隔默认 5 分钟。
pub const DEFAULT_ALERT_SECONDS: f64 = 300.0;
pub const MIN_ALERT_SECONDS: f64 = 30.0;
pub const MAX_ALERT_SECONDS: f64 = 3600.0;
impl Settings {
    /// 三个阈值按 `monitor::Alert::ALL` 的顺序排好，面板直接按下标取。
    pub fn thresholds(&self) -> [u8; 3] {
        [self.alert_cpu, self.alert_memory, self.alert_disk]
    }
    pub fn alert_cooldown(&self) -> std::time::Duration {
        let seconds = if self.alert_seconds.is_finite() && self.alert_seconds > 0.0 {
            self.alert_seconds
        } else {
            DEFAULT_ALERT_SECONDS
        };
        std::time::Duration::from_secs_f64(seconds.clamp(MIN_ALERT_SECONDS, MAX_ALERT_SECONDS))
    }
    pub fn limits(&self) -> crate::monitor::Limits {
        let seconds = |value: f64, default: std::time::Duration| {
            if value.is_finite() && value > 0.0 { std::time::Duration::from_secs_f64(value) } else { default }
        };
        crate::monitor::Limits {
            interval: seconds(self.interval_seconds, crate::monitor::DEFAULT_INTERVAL),
            history: seconds(self.history_seconds, crate::monitor::DEFAULT_HISTORY),
            disk_interval: seconds(self.disk_seconds, crate::monitor::DEFAULT_DISK_INTERVAL),
        }
    }
}

pub(super) fn read(r: &mut Reader<'_>) -> Settings {
    let mut settings = Settings::default();
    if let Some(enabled) = r.boolean("monitor", "enabled") {
        settings.enabled = enabled;
    }
    if let Some(seconds) = r.number(
        "monitor",
        "interval-seconds",
        crate::monitor::MIN_INTERVAL.as_secs_f64(),
        crate::monitor::MAX_INTERVAL.as_secs_f64(),
    ) {
        settings.interval_seconds = seconds;
    }
    if let Some(seconds) = r.number(
        "monitor",
        "history-seconds",
        crate::monitor::MIN_HISTORY.as_secs_f64(),
        crate::monitor::MAX_HISTORY.as_secs_f64(),
    ) {
        settings.history_seconds = seconds;
    }
    if let Some(seconds) = r.number(
        "monitor",
        "disk-seconds",
        crate::monitor::MIN_DISK_INTERVAL.as_secs_f64(),
        crate::monitor::MAX_DISK_INTERVAL.as_secs_f64(),
    ) {
        settings.disk_seconds = seconds;
    }
    for (key, target) in [
        ("alert-cpu", &mut settings.alert_cpu),
        ("alert-memory", &mut settings.alert_memory),
        ("alert-disk", &mut settings.alert_disk),
    ] {
        // 0 表示关闭；上限 100 是因为读数本身就是百分比。
        if let Some(percent) = r.number("monitor", key, 0.0, 100.0) {
            *target = percent.round().clamp(0.0, 100.0) as u8;
        }
    }
    if let Some(seconds) = r.number("monitor", "alert-seconds", MIN_ALERT_SECONDS, MAX_ALERT_SECONDS) {
        settings.alert_seconds = seconds;
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_overrides_stay_inside_the_fixed_limits() {
        let (config, warnings) = Config::parse("");
        assert!(warnings.is_empty());
        assert_eq!(config.monitor, Settings::default());
        assert!(config.monitor.enabled);
        assert_eq!(config.monitor.limits().samples(), 300);

        let (config, warnings) = Config::parse(
            "[monitor]\nenabled = false\ninterval-seconds = 2\nhistory-seconds = 600\ndisk-seconds = 30\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!config.monitor.enabled);
        assert_eq!(config.monitor.interval_seconds, 2.0);
        assert_eq!(config.monitor.history_seconds, 600.0);
        assert_eq!(config.monitor.disk_seconds, 30.0);
        let limits = config.monitor.limits();
        assert_eq!(limits.interval, std::time::Duration::from_secs(2));
        assert_eq!(limits.disk_interval, std::time::Duration::from_secs(30));
        assert_eq!(limits.samples(), 300);
    }
    #[test]
    fn alert_thresholds_parse_clamp_and_default_to_off() {
        // 默认全关：不替用户猜阈值，也不在升级后突然开始弹通知。
        let (config, warnings) = Config::parse("");
        assert!(warnings.is_empty());
        assert_eq!(config.monitor.thresholds(), [0, 0, 0]);
        assert_eq!(config.monitor.alert_cooldown(), std::time::Duration::from_secs(300));

        let (config, warnings) =
            Config::parse("[monitor]\nalert-cpu = 90\nalert-memory = 85.6\nalert-disk = 0\nalert-seconds = 60\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.monitor.thresholds(), [90, 86, 0]);
        assert_eq!(config.monitor.alert_cooldown(), std::time::Duration::from_secs(60));

        // 越界与非法值都保留默认（关闭），并给出警告。
        let (config, warnings) =
            Config::parse("[monitor]\nalert-cpu = 150\nalert-memory = -1\nalert-disk = \"high\"\nalert-seconds = 5\n");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert_eq!(config.monitor.thresholds(), [0, 0, 0]);
        assert_eq!(config.monitor.alert_cooldown(), std::time::Duration::from_secs(300));
        // 直接构造越界冷却时间时按上下限夹紧，不会变成 0 或无限刷。
        let settings = Settings { alert_seconds: 1.0, ..Settings::default() };
        assert_eq!(settings.alert_cooldown(), std::time::Duration::from_secs(30));
        let settings = Settings { alert_seconds: 1e9, ..Settings::default() };
        assert_eq!(settings.alert_cooldown(), std::time::Duration::from_secs(3600));
    }
    #[test]
    fn out_of_range_values_warn_and_keep_the_defaults() {
        let (config, warnings) =
            Config::parse("[monitor]\ninterval-seconds = 0.2\nhistory-seconds = 5000\ndisk-seconds = 1\n");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(config.monitor, Settings::default());
        // 负数与非数字同样保留默认值。
        let (config, warnings) = Config::parse("[monitor]\ninterval-seconds = -3\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(config.monitor.interval_seconds, 1.0);
    }
}
