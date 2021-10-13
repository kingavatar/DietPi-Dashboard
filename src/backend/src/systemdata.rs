use futures::StreamExt;
use heim::{
    cpu, disk, host, memory, net, process,
    units::{
        information::{byte, mebibyte},
        ratio::percent,
        time::second,
    },
};
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::str::from_utf8;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{thread, time};

use crate::types;

// Use u64::MAX to originally set traffic
lazy_static! {
    static ref BYTES_SENT: AtomicU64 = AtomicU64::new(u64::MAX);
    static ref BYTES_RECV: AtomicU64 = AtomicU64::new(u64::MAX);
}

#[allow(clippy::cast_possible_truncation)]
fn round_percent(unrounded: f64) -> f32 {
    ((unrounded * 100.0).round() / 100.0) as f32
}

pub async fn cpu() -> f32 {
    let times1 = cpu::time().await.unwrap();
    let used1 = times1.system() + times1.user();
    thread::sleep(time::Duration::from_secs(1));
    let times2 = cpu::time().await.unwrap();
    let used2 = times2.system() + times2.user();

    round_percent(
        ((used2 - used1) / ((used2 + times2.idle()) - (used1 + times1.idle()))).get::<percent>(),
    )
}

#[allow(clippy::cast_precision_loss)]
pub async fn ram() -> types::UsageData {
    let ram = memory::memory().await.unwrap();
    let ram_used = (ram.total() - ram.available()).get::<byte>();
    let ram_total = ram.total().get::<byte>();

    types::UsageData {
        used: ram_used,
        total: ram_total,
        percent: round_percent((ram_used as f64 / ram_total as f64) * 100.0),
    }
}

#[allow(clippy::cast_precision_loss)]
pub async fn swap() -> types::UsageData {
    let swap = memory::swap().await.unwrap();
    let swap_used = swap.used().get::<byte>();
    let swap_total = swap.total().get::<byte>();

    types::UsageData {
        used: swap_used,
        total: swap_total,
        percent: if swap_total == 0 {
            0.0
        } else {
            round_percent((swap_used as f64 / swap_total as f64) * 100.0)
        },
    }
}

pub async fn disk() -> types::UsageData {
    let disk = disk::usage("/").await.unwrap();

    types::UsageData {
        used: disk.used().get::<byte>(),
        total: disk.total().get::<byte>(),
        percent: round_percent(disk.ratio().get::<percent>().into()),
    }
}

pub async fn network() -> types::NetData {
    // Get data from all interfaces
    let (sent, recv) = net::io_counters()
        .fold((0_u64, 0_u64), |accumulated, val| async move {
            let unwrapped = val.unwrap();
            (
                accumulated.0 + unwrapped.bytes_sent().get::<byte>(),
                accumulated.1 + unwrapped.bytes_recv().get::<byte>(),
            )
        })
        .await;

    let data = types::NetData {
        recieved: recv.saturating_sub(BYTES_RECV.load(Relaxed)),
        sent: sent.saturating_sub(BYTES_SENT.load(Relaxed)),
    };

    BYTES_SENT.store(sent, Relaxed);
    BYTES_RECV.store(recv, Relaxed);

    data
}

pub async fn processes() -> Vec<types::ProcessData> {
    let processes = process::processes()
        .map(Result::unwrap)
        .collect::<Vec<process::Process>>()
        .await;
    let mut process_list = Vec::new();
    let mut cpu_list: HashMap<i32, process::CpuUsage> = HashMap::new();
    process_list.reserve(processes.len());
    for element in &processes {
        cpu_list.insert(element.pid(), element.cpu_usage().await.unwrap());
    }
    thread::sleep(time::Duration::from_millis(500));
    for element in processes {
        // Name could fail if the process terminates, if so skip the process
        let name;
        match element.name().await {
            Ok(unwrapped_name) => name = unwrapped_name,
            Err(_) => continue,
        }
        let status: String;
        match element.status().await.unwrap() {
            // The proceses that are running show up as sleeping, for some reason
            process::Status::Sleeping => status = "running".to_string(),
            process::Status::Idle => status = "idle".to_string(),
            process::Status::Stopped => status = "stopped".to_string(),
            process::Status::Zombie => status = "zombie".to_string(),
            process::Status::Dead => status = "dead".to_string(),
            _ => status = String::new(),
        }
        let pid = element.pid();
        process_list.push(types::ProcessData {
            pid,
            name,
            cpu: round_percent(
                (element.cpu_usage().await.unwrap() - cpu_list.remove(&pid).unwrap())
                    .get::<percent>()
                    .into(),
            ),
            ram: element.memory().await.unwrap().vms().get::<mebibyte>(),
            status,
        });
    }
    process_list
}

pub fn dpsoftware() -> Vec<types::DPSoftwareData> {
    let out = Command::new("/boot/dietpi/dietpi-software")
        .arg("list")
        .output()
        .unwrap()
        .stdout;
    let out_list = from_utf8(&out).unwrap().split('\n').collect::<Vec<&str>>();
    let mut software_list = Vec::new();
    software_list.reserve(match out_list.len().checked_sub(9) {
        Some(num) => num,
        None => return software_list,
    });
    'software: for element in out_list.iter().skip(4).take(out_list.len() - 5) {
        let mut id = 0;
        let mut installed = false;
        let mut name = String::new();
        let mut docs = String::new();
        let mut depends = String::new();
        let mut desc = String::new();
        for (in1, el1) in element.split('|').enumerate() {
            match in1 {
                0 => {
                    id = el1
                        .trim()
                        .trim_start_matches("\u{001b}[32m")
                        .trim_start_matches("ID ")
                        .parse::<i16>()
                        .unwrap();
                }
                1 => installed = el1.trim().trim_start_matches('=').parse::<i8>().unwrap() > 0,
                2 => {
                    let mut name_desc = el1.trim().split(':');
                    name = name_desc.next().unwrap().to_string();
                    desc = name_desc
                        .next()
                        .unwrap()
                        .trim_start_matches("\u{001b}[0m \u{001b}[90m")
                        .trim_end_matches("\u{001b}[0m")
                        .to_string();
                }
                3 => {
                    if el1.contains("DISABLED") {
                        software_list.push(types::DPSoftwareData {
                            id: -1,
                            installed: false,
                            name: String::new(),
                            description: String::new(),
                            dependencies: String::new(),
                            docs: String::new(),
                        });
                        continue 'software;
                    }
                    depends = el1.trim().to_string();
                }
                4 => {
                    docs = el1
                        .trim()
                        .trim_start_matches("\u{001b}[90m")
                        .trim_end_matches("\u{001b}[0m")
                        .to_string();
                }
                _ => {}
            }
        }
        software_list.push(types::DPSoftwareData {
            id,
            dependencies: depends,
            docs,
            name,
            description: desc,
            installed,
        });
    }
    software_list
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub async fn host() -> types::HostData {
    let info = host::platform().await.unwrap();
    let uptime = host::uptime().await.unwrap().get::<second>().round() as u64;
    let dp_file = fs::read_to_string(&std::path::Path::new("/boot/dietpi/.version")).unwrap();
    let dp_version: Vec<&str> = dp_file.split(&['=', '\n'][..]).collect();
    let installed_pkgs = from_utf8(
        &Command::new("dpkg")
            .arg("--get-selections")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .lines()
    .count();
    let upgradable_pkgs = fs::read_to_string("/run/dietpi/.apt_updates")
        .unwrap_or_else(|_| 0.to_string())
        .trim_end_matches('\n')
        .parse::<u32>()
        .unwrap();
    let mut arch = info.architecture().as_str();
    if arch == "unknown" {
        arch = "armv6/other";
    } else if arch == "arm" {
        arch = "armv7";
    }
    // Skip loopback MAC, loopback IP, and interface MAC
    let nic = net::nic().skip(3).next().await.unwrap().unwrap();

    let mut ip = String::new();
    match nic.address() {
        net::Address::Inet(addr) | net::Address::Inet6(addr) => ip = addr.ip().to_string(),
        _ => (),
    }
    types::HostData {
        hostname: info.hostname().to_string(),
        uptime,
        arch: arch.to_string(),
        kernel: info.release().to_string(),
        version: format!("{}.{}.{}", dp_version[1], dp_version[3], dp_version[5]),
        packages: installed_pkgs,
        upgrades: upgradable_pkgs,
        nic: nic.name().to_string(),
        ip,
    }
}

pub fn services() -> Vec<types::ServiceData> {
    let services = &Command::new("/boot/dietpi/dietpi-services")
        .arg("status")
        .output()
        .unwrap()
        .stdout;
    let services_str = from_utf8(services).unwrap();
    let mut services_list = Vec::new();
    for element in services_str
        .replace("[FAILED] DietPi-Services | \u{25cf} ", "dpdashboardtemp")
        .replace("[ INFO ] DietPi-Services | ", "dpdashboardtemp")
        .replace("[  OK  ] DietPi-Services | ", "dpdashboardtemp")
        .split("dpdashboardtemp")
        .skip(1)
    {
        let mut name = String::new();
        let mut log = String::new();
        let mut status = String::new();
        let mut start = String::new();
        if element.contains(".service") {
            for (index, el1) in element.split('\n').enumerate() {
                status = "failed".to_string();
                match index {
                    0 => name = el1.split_once(".service").unwrap().0.to_string(),
                    9.. => log.push_str(format!("{}<br>", el1).as_str()),
                    _ => (),
                }
            }
        } else {
            let (el1, el2) = element.split_once(':').unwrap();
            name = el1.trim().to_string();
            match el2.split_once(" since ") {
                Some(statusdate) => {
                    match statusdate.0.trim() {
                        "active (running)" => status = "running".to_string(),
                        "active (exited)" => status = "exited".to_string(),
                        "inactive (dead)" => status = "dead".to_string(),
                        _ => status = "unknown".to_string(),
                    }
                    start = statusdate.1.trim().to_string();
                }
                None => status = "dead".to_string(),
            }
        }
        services_list.push(types::ServiceData {
            name,
            log,
            status,
            start,
        });
    }
    services_list
}

pub fn global() -> types::GlobalData {
    let update =
        fs::read_to_string("/run/dietpi/.update_available").unwrap_or_else(|_| String::new());
    types::GlobalData { update }
}