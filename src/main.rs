use diqwest::WithDigestAuth;
use serde::Deserialize;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{BufReader, Read};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use surge_ping::{Client, Config, PingIdentifier, PingSequence};
use tokio::spawn;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Deserialize, Debug, Clone)]
struct FritzBoxConfig {
    username: String,
    password: String,
    #[serde(default = "default_scheme")]
    scheme:  String,
    #[serde(default = "default_host")]
    host: String,
    #[serde(default = "default_port")]
    port: u16,
}

fn default_scheme() -> String {
    "http".to_string()
}

fn default_host() -> String {
    "192.168.178.1".to_string()
}

fn default_port() -> u16 {
    49000
}

#[derive(Deserialize, Debug, Clone)]
struct PingTarget {
    name: String,
    target_ip: IpAddr,
}

#[derive(Deserialize, Debug, Clone)]
struct Configuration {
    #[serde(default = "default_failure_threshold")]
    failure_threshold: u64,
    #[serde(default = "default_ping_interval_secs")]
    ping_interval_secs: u64,
    ping_targets: Vec<PingTarget>,
    fritz_box_config: FritzBoxConfig,
}

fn default_failure_threshold() -> u64 {
    3
}

fn default_ping_interval_secs() -> u64 {
    60
}

fn cfg() -> &'static Configuration {
    static CONFIG: OnceLock<Configuration> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mut config_file_reader = BufReader::new(
            OpenOptions::new()
                .read(true)
                .open(&std::env::args().collect::<Vec<_>>()[1])
                .expect("configuration file must exist")
        );
        let mut config_file_str = String::new();
        config_file_reader.read_to_string(&mut config_file_str).expect("must be able to read config fil as UTF-8 string");

        toml::from_str(&config_file_str).expect("config file must be a valid TOML file")
    })
}

struct Task {
    name: String,
    client: Arc<Client>,
    dest: IpAddr,
    failure_count: Arc<AtomicU64>,
    ping_identifier: PingIdentifier,
}

impl Task {
    async fn run(self) {
        let dest = self.dest.clone();
        if let Err(e) = self.inner_run().await {
            error!(dest=dest.to_string(), error=e, "encountered error running ping")
        }
    }

    async fn inner_run(self) -> Result<(), Box<dyn Error>> {
        info!(name=self.name, dest=self.dest.to_string(), "creating pinger");
        let mut pinger = self.client
            .pinger(
                self.dest.clone(),
                self.ping_identifier,
            )
            .await;
        info!(name=self.name, dest=self.dest.to_string(), "created pinger, starting loop");

        let mut seq = 0u16;
        loop {
            debug!(name=self.name, dest=self.dest.to_string(), "executing ping");
            match pinger.ping(PingSequence(seq), &Vec::new()).await {
                Ok(_) => {
                    debug!(name=self.name, dest=self.dest.to_string(), "ping successful");
                    if self.failure_count.load(Ordering::SeqCst) > 0 {
                        info!(name=self.name, dest=self.dest.to_string(), "ping recovered");
                    }
                    self.failure_count.store(0, Ordering::SeqCst);
                },
                Err(e) => {
                    error!(name=self.name, dest=self.dest.to_string(), error=e.to_string(), "failure pinging");
                    self.failure_count.fetch_add(1, Ordering::SeqCst);
                },
            }

            if seq == u16::MAX {
                seq = 0;
            } else {
                seq += 1;
            }

            debug!(name=self.name, dest=self.dest.to_string(), sleep_duration_secs=cfg().ping_interval_secs, "sleeping before next ping");
            sleep(Duration::from_secs(cfg().ping_interval_secs)).await;
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let mut handles = Vec::new();
    let config = Config::builder()
        .ttl(56)
        .build();
    let client = Arc::new(Client::new(&config)
        .expect("client should create successfully"));

    if cfg().ping_targets.is_empty() {
        panic!("must provide at least 1 ping target (3 or more recommended)");
    }

    if cfg().ping_targets.len() < 3 {
        warn!("strongly recommend providing at least three ping targets, otherwise a single target having issues will result in an unnecessary restart");
    }

    if cfg().ping_targets.len() % 2 == 0 {
        info!("even number of ping targets might result in unexpected behaviour as it looks for more than 50% of ping_targets exceeding failure_threshold");
    }

    let mut failure_counts = Vec::new();
    for ping_target in &cfg().ping_targets {
        info!("Creating and spawning {} checker", ping_target.name);
        match ping_target.target_ip {
            IpAddr::V4(ip) => info!(ip=ip.to_string(), "target is IPv4"),
            IpAddr::V6(ip) => info!(ip=ip.to_string(), "target is IPv6"),
        }
        let count = Arc::new(AtomicU64::new(0));
        failure_counts.push(Arc::clone(&count));
        let task = Task {
            name: ping_target.name.clone(),
            client: Arc::clone(&client),
            dest: ping_target.target_ip.clone(),
            failure_count: Arc::clone(&count),
            ping_identifier: PingIdentifier(3),
        };
        handles.push(spawn(async move {
            task.run().await
        }));
    }

    info!("Creating and spawning Fritz!Box restarter");
    handles.push(spawn( async move {
        let http_client = reqwest::Client::builder().build().expect("http client should build without error");
        // let failure_counts = vec![google_count, cloudflare_count, control_d_count];
        let mut consecutive_restarts = 0u64;
        loop {
            debug!(host_count=failure_counts.len(), "checking if > 50% of hosts failed ping checks");
            let host_failures_over_threshold = failure_counts
                .iter()
                .filter(|it| it.load(Ordering::SeqCst) >= cfg().failure_threshold)
                .count();
            debug!(host_count=failure_counts.len(), host_failures_over_threshold, "checked hosts for failures");

            if host_failures_over_threshold > ((failure_counts.len() as f64) / 2.0).ceil() as usize {
                error!(host_count=failure_counts.len(), "more than 50% of hosts reported {} or more consecutive ping errors - assuming connection failed and restarting Fritz!Box", cfg().failure_threshold);
                consecutive_restarts += 1;
                let request = http_client
                    .post(format!("{}://{}:{}/upnp/control/deviceconfig", cfg().fritz_box_config.scheme, cfg().fritz_box_config.host, cfg().fritz_box_config.port))
                    .header("Content-Type", "text/xml; charset=\"utf-8\"")
                    .header("SoapAction", "urn:dslforum-org:service:DeviceConfig:1#Reboot")
                    .body(r#"<?xml version='1.0' encoding='utf-8'?><s:Envelope s:encodingStyle='http://schemas.xmlsoap.org/soap/encoding/' xmlns:s='http://schemas.xmlsoap.org/soap/envelope/'><s:Body><u:Reboot xmlns:u='urn:dslforum-org:service:DeviceConfig:1'></u:Reboot></s:Body></s:Envelope>"#)
                    .send_with_digest_auth(&cfg().fritz_box_config.username, &cfg().fritz_box_config.password)
                    .await;

                match request {
                    Ok(resp) => {
                        if resp.status().is_success() {
                            info!("Fritz!Box restart successful, sleeping for 300 seconds to give the restart time to complete and other ping commands a chance to recover");
                            sleep(Duration::from_secs(300)).await;
                        } else {
                            let response_bytes = resp.bytes().await.expect("should have received response content from Fritz!Box");
                            let from_utf8lossy = String::from_utf8_lossy(&response_bytes);
                            let resp_content = from_utf8lossy.as_ref();
                            error!(
                                err=resp_content,
                                "received error response from Fritz!Box",
                            )
                        }
                    }
                    Err(e) => {
                        error!(error=e.to_string(), "encountered error communicating with Fritz!Box")
                    }
                }
            } else {
                debug!(host_count=failure_counts.len(), host_failures_over_threshold, "<50% of hosts are failing, assuming connectivity is fine and resetting consecutive_restart count");
                consecutive_restarts = 0;
            }

            // 2^30^2*60 is greater than 2^64, so let's avoid any issues with integer overflow, yeah?
            if consecutive_restarts >= 2u64.pow(29) {
                error!("have somehow made 2^29 attempts to restart without success, assuming cosmic bit flips or something?");
                consecutive_restarts = 2;
            }

            if consecutive_restarts > 1 {
                let secs = 60 * consecutive_restarts.pow(2);
                error!(consecutive_restarts, sleep_duration_secs=secs, "Excessive number of consecutive restarts - maybe an ISP issue, so backing off");
                sleep(Duration::from_secs(secs)).await;
            } else {
                debug!(sleep_secs=60, "sleeping before checking for host failures");
                sleep(Duration::from_secs(60)).await;
            }
        }
    }));

    futures_util::future::join_all(handles).await;

    warn!("all tasks have finished - this is unlikely to be the right thing to do");
}
