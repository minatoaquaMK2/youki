use anyhow::{bail, Context, Result};
use std::{borrow::Cow, path::Path};

use crate::{
    common::{self, ControllerOpt},
    stats::{self, CpuStats, StatsProvider},
};

use oci_spec::runtime::LinuxCpu;

use super::controller::Controller;

const CGROUP_CPU_WEIGHT: &str = "cpu.weight";
const CGROUP_CPU_MAX: &str = "cpu.max";
const CGROUP_CPU_BURST: &str = "cpu.max.burst";
const CGROUP_CPU_IDLE: &str = "cpu.idle";
const UNRESTRICTED_QUOTA: &str = "max";
const MAX_CPU_WEIGHT: u64 = 10000;

const CPU_STAT: &str = "cpu.stat";
const CPU_PSI: &str = "cpu.pressure";

pub struct Cpu {}

impl Controller for Cpu {
    fn apply(controller_opt: &ControllerOpt, path: &Path) -> Result<()> {
        if let Some(cpu) = &controller_opt.resources.cpu() {
            Self::apply(path, cpu).context("failed to apply cpu resource restrictions")?;
        }

        Ok(())
    }
}

impl StatsProvider for Cpu {
    type Stats = CpuStats;

    fn stats(cgroup_path: &Path) -> Result<Self::Stats> {
        let mut stats = CpuStats::default();

        let stat_content = common::read_cgroup_file(cgroup_path.join(CPU_STAT))?;
        for entry in stat_content.lines() {
            let parts: Vec<&str> = entry.split_ascii_whitespace().collect();
            if parts.len() != 2 {
                continue;
            }

            let value = parts[1].parse()?;
            match parts[0] {
                "usage_usec" => stats.usage.usage_total = value,
                "user_usec" => stats.usage.usage_user = value,
                "system_usec" => stats.usage.usage_kernel = value,
                _ => continue,
            }
        }

        stats.psi =
            stats::psi_stats(&cgroup_path.join(CPU_PSI)).context("could not read cpu psi")?;
        Ok(stats)
    }
}

impl Cpu {
    fn apply(path: &Path, cpu: &LinuxCpu) -> Result<()> {
        if Self::is_realtime_requested(cpu) {
            bail!("realtime is not supported on cgroup v2 yet");
        }

        if let Some(mut shares) = cpu.shares() {
            shares = Self::convert_shares_to_cgroup2(shares);
            if shares != 0 {
                // will result in Erno 34 (numerical result out of range) otherwise
                common::write_cgroup_file(path.join(CGROUP_CPU_WEIGHT), shares)?;
            }
        }

        let cpu_max_file = path.join(CGROUP_CPU_MAX);
        let new_cpu_max: Option<Cow<str>> = match (cpu.quota(), cpu.period()) {
            (None, Some(period)) => Self::create_period_only_value(&cpu_max_file, period)?,
            (Some(quota), None) if quota > 0 => Some(quota.to_string().into()),
            (Some(quota), None) if quota <= 0 => Some(UNRESTRICTED_QUOTA.into()),
            (Some(quota), Some(period)) if quota > 0 => Some(format!("{quota} {period}").into()),
            (Some(quota), Some(period)) if quota <= 0 => {
                Some(format!("{UNRESTRICTED_QUOTA} {period}").into())
            }
            _ => None,
        };

        // format is 'quota period'
        // the kernel default is 'max 100000'
        // 250000 250000 -> 1 CPU worth of runtime every 250ms
        // 10000 50000 -> 20% of one CPU every 50ms
        if let Some(cpu_max) = new_cpu_max {
            common::write_cgroup_file_str(&cpu_max_file, &cpu_max)?;
        }

        if let Some(burst) = cpu.burst() {
            common::write_cgroup_file(path.join(CGROUP_CPU_BURST), burst)?;
        }

        if let Some(idle) = cpu.idle() {
            common::write_cgroup_file(path.join(CGROUP_CPU_IDLE), idle)?;
        }

        Ok(())
    }

    fn convert_shares_to_cgroup2(shares: u64) -> u64 {
        if shares == 0 {
            return 0;
        }

        let weight = 1 + ((shares.saturating_sub(2)) * 9999) / 262142;
        weight.min(MAX_CPU_WEIGHT)
    }

    fn is_realtime_requested(cpu: &LinuxCpu) -> bool {
        if cpu.realtime_period().is_some() {
            return true;
        }

        if cpu.realtime_runtime().is_some() {
            return true;
        }

        false
    }

    fn create_period_only_value(cpu_max_file: &Path, period: u64) -> Result<Option<Cow<str>>> {
        let old_cpu_max = common::read_cgroup_file(cpu_max_file)?;
        if let Some(old_quota) = old_cpu_max.split_whitespace().next() {
            return Ok(Some(format!("{old_quota} {period}").into()));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        stats::CpuUsage,
        test::{create_temp_dir, set_fixture, setup},
    };
    use oci_spec::runtime::LinuxCpuBuilder;
    use std::fs;

    #[test]
    fn test_set_valid_shares() {
        // arrange
        let (tmp, weight) = setup("test_set_shares", CGROUP_CPU_WEIGHT);
        let _ = set_fixture(&tmp, CGROUP_CPU_MAX, "")
            .unwrap_or_else(|_| panic!("set test fixture for {CGROUP_CPU_MAX}"));
        let cpu = LinuxCpuBuilder::default().shares(22000u64).build().unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(weight)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_WEIGHT} file content"));
        assert_eq!(content, 840.to_string());
    }

    #[test]
    fn test_set_cpu_idle() {
        // arrange
        const IDLE: i64 = 1;
        const CPU: &str = "cpu";

        if !Path::new(common::DEFAULT_CGROUP_ROOT)
            .join(CPU)
            .join(CGROUP_CPU_IDLE)
            .exists()
        {
            // skip test_set_cpu_idle due to not found cpu.idle, maybe due to old kernel version
            return;
        }

        let (tmp, max) = setup("test_set_cpu_idle", CGROUP_CPU_IDLE);
        let cpu = LinuxCpuBuilder::default().idle(IDLE).build().unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(max)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_IDLE} file content"));
        assert_eq!(content, format!("{IDLE}"))
    }

    #[test]
    fn test_set_positive_quota() {
        // arrange
        const QUOTA: i64 = 200000;
        let (tmp, max) = setup("test_set_positive_quota", CGROUP_CPU_MAX);
        let cpu = LinuxCpuBuilder::default().quota(QUOTA).build().unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(max)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_MAX} file content"));
        assert_eq!(content, format!("{QUOTA}"))
    }

    #[test]
    fn test_set_negative_quota() {
        // arrange
        let (tmp, max) = setup("test_set_negative_quota", CGROUP_CPU_MAX);
        let cpu = LinuxCpuBuilder::default().quota(-500).build().unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(max)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_MAX} file content"));
        assert_eq!(content, UNRESTRICTED_QUOTA)
    }

    #[test]
    fn test_set_positive_period() {
        // arrange
        const QUOTA: u64 = 50000;
        const PERIOD: u64 = 100000;
        let (tmp, max) = setup("test_set_positive_period", CGROUP_CPU_MAX);
        common::write_cgroup_file(&max, QUOTA).unwrap();
        let cpu = LinuxCpuBuilder::default().period(PERIOD).build().unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(max)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_MAX} file content"));
        assert_eq!(content, format!("{QUOTA} {PERIOD}"))
    }

    #[test]
    fn test_set_quota_and_period() {
        // arrange
        const QUOTA: i64 = 200000;
        const PERIOD: u64 = 100000;
        let (tmp, max) = setup("test_set_quota_and_period", CGROUP_CPU_MAX);
        let cpu = LinuxCpuBuilder::default()
            .quota(QUOTA)
            .period(PERIOD)
            .build()
            .unwrap();

        // act
        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        // assert
        let content = fs::read_to_string(max)
            .unwrap_or_else(|_| panic!("read {CGROUP_CPU_MAX} file content"));
        assert_eq!(content, format!("{QUOTA} {PERIOD}"));
    }

    #[test]
    fn test_realtime_runtime_not_supported() {
        // arrange
        let tmp = create_temp_dir("test_realtime_runtime_not_supported")
            .expect("create temp directory for test");
        let cpu = LinuxCpuBuilder::default()
            .realtime_runtime(5)
            .build()
            .unwrap();

        // act
        let result = Cpu::apply(&tmp, &cpu);

        // assert
        assert!(
            result.is_err(),
            "realtime runtime is not supported and should return an error"
        );
    }

    #[test]
    fn test_realtime_period_not_supported() {
        // arrange
        let tmp = create_temp_dir("test_realtime_period_not_supported")
            .expect("create temp directory for test");
        let cpu = LinuxCpuBuilder::default()
            .realtime_period(5u64)
            .build()
            .unwrap();

        // act
        let result = Cpu::apply(&tmp, &cpu);

        // assert
        assert!(
            result.is_err(),
            "realtime period is not supported and should return an error"
        );
    }

    #[test]
    fn test_stat_usage() {
        let tmp = create_temp_dir("test_stat_usage").expect("create temp directory for test");
        let content = ["usage_usec 7730", "user_usec 4387", "system_usec 3498"].join("\n");
        set_fixture(&tmp, CPU_STAT, &content).expect("create stat file");
        set_fixture(&tmp, CPU_PSI, "").expect("create psi file");

        let actual = Cpu::stats(&tmp).expect("get cgroup stats");
        let expected = CpuUsage {
            usage_total: 7730,
            usage_user: 4387,
            usage_kernel: 3498,
            ..Default::default()
        };

        assert_eq!(actual.usage, expected);
    }

    #[test]
    fn test_burst() {
        let expected = 100000u64;
        let (tmp, burst_file) = setup("test_burst", CGROUP_CPU_BURST);
        let cpu = LinuxCpuBuilder::default().burst(expected).build().unwrap();

        Cpu::apply(&tmp, &cpu).expect("apply cpu");

        let actual = fs::read_to_string(burst_file).expect("read burst file");
        assert_eq!(actual, expected.to_string());
    }
}
