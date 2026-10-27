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
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: crate::monitor::DEFAULT_INTERVAL.as_secs_f64(),
            history_seconds: crate::monitor::DEFAULT_HISTORY.as_secs_f64(),
            disk_seconds: crate::monitor::DEFAULT_DISK_INTERVAL.as_secs_f64(),
        }
    }
}
impl Settings {
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
