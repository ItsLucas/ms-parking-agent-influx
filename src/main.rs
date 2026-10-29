use anyhow::{Context, Result};
use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Asia::Shanghai;
use config::{Config, File};
use futures::stream;
use influxdb2::Client;
use influxdb2::models::DataPoint;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

#[derive(Debug, Deserialize)]
struct AppConfig {
    api: ApiConfig,
    influxdb: InfluxDbConfig,
}

#[derive(Debug, Deserialize)]
struct ApiConfig {
    url: String,
    scraping_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
struct InfluxDbConfig {
    url: String,
    org: String,
    bucket: String,
    token: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiResponse {
    success: bool,
    #[serde(rename = "msparkingData")]
    msparking_data: Vec<AreaData>,
    date: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct AreaData {
    #[serde(rename = "areaCode")]
    area_code: i32,
    #[serde(rename = "areaFreeSpaceNum")]
    area_free_space_num: i64,
}

async fn load_config() -> Result<AppConfig> {
    let config = Config::builder()
        .add_source(File::with_name("config/default"))
        .build()
        .context("Failed to load configuration")?;
    
    config.try_deserialize::<AppConfig>()
        .context("Failed to deserialize configuration")
}

// Every network call must be bounded: a single stalled TCP connection (peer silently gone,
// NAT/route change) otherwise parks the loop forever while systemd still reports "running".
const API_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const API_TIMEOUT: Duration = Duration::from_secs(20);
const INFLUX_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

fn build_api_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(API_CONNECT_TIMEOUT)
        .timeout(API_TIMEOUT)
        .tcp_keepalive(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .context("Failed to build API HTTP client")
}

async fn fetch_parking_data(http: &reqwest::Client, url: &str) -> Result<ApiResponse> {
    let response = http.get(url)
        .send()
        .await
        .context("Failed to send request")?
        .error_for_status()
        .context("API returned HTTP error")?;
    
    let data = response.json::<ApiResponse>()
        .await
        .context("Failed to parse API response")?;
    
    Ok(data)
}

fn is_in_maintenance_window() -> bool {
    let shanghai_time: DateTime<chrono_tz::Tz> = Utc::now().with_timezone(&Shanghai);
    let hour = shanghai_time.hour();
    let minute = shanghai_time.minute();
    
    (hour == 23 && minute >= 50) || (hour == 0 && minute < 20)
}

/// Mirrors the official page's mapping (https://ms.suzhouparking.kingfaster.com.cn/ getData()).
/// B3/B5 lots have been re-numbered upstream (4 -> 18, 6 -> 19); both codes map to the same
/// location while the area_code tag keeps the raw source identity.
fn location_for(area_code: i32) -> &'static str {
    match area_code {
        12 => "SIP-B25-B26", // B25 & B26地库, 508 spaces
        2 => "ZHONGMENG",    // 中盟地库, 205 spaces
        4 | 18 => "SIP-B3",  // B3地库, 20 spaces
        6 | 19 => "SIP-B5",  // B5地库, 20 spaces
        _ => "Unknown",
    }
}

fn create_data_point(area: &AreaData) -> DataPoint {
    let now = Utc::now();
    
    DataPoint::builder("parking_spaces")
        .tag("area_code", area.area_code.to_string())
        .tag("location", location_for(area.area_code))
        .field("free_spaces", area.area_free_space_num)
        .timestamp(now.timestamp_nanos_opt().unwrap())
        .build()
        .unwrap()
}

/// Keep the last non-zero reading per area, but only for areas the API still reports:
/// retired codes (e.g. 4/6 after the B3/B5 renumbering) must not be replayed forever
/// during the maintenance window.
fn update_cache(cache: &mut HashMap<i32, AreaData>, areas: &[AreaData]) {
    cache.retain(|code, _| areas.iter().any(|a| a.area_code == *code));
    for area in areas {
        if area.area_free_space_num > 0 {
            cache.insert(area.area_code, area.clone());
        }
    }
}

async fn write_points(client: &Client, bucket: &str, data_points: Vec<DataPoint>) -> Result<()> {
    // influxdb2 0.5 owns its reqwest 0.11 client with no timeout; bound the whole write here.
    // Dropping the future on timeout drops the hung connection, so the next write dials fresh.
    time::timeout(INFLUX_WRITE_TIMEOUT, client.write(bucket, stream::iter(data_points)))
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {:?}", INFLUX_WRITE_TIMEOUT))?
        .map_err(|e| anyhow::anyhow!(e))
}

async fn run_scraper(config: AppConfig) -> Result<()> {
    let client = Arc::new(Client::new(&config.influxdb.url, &config.influxdb.org, &config.influxdb.token));
    let http = build_api_client()?;
    
    let mut interval = time::interval(Duration::from_secs(config.api.scraping_interval_secs));
    // After a slow tick, don't fire a burst of catch-up scrapes; just resume the cadence.
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    
    let mut cached_data: HashMap<i32, AreaData> = HashMap::new();
    
    info!("Starting parking data scraper. Interval: {} seconds", config.api.scraping_interval_secs);
    
    loop {
        interval.tick().await;
        info!("Fetching parking data...");
        
        let using_cache = is_in_maintenance_window();
        if using_cache {
            info!("Currently in maintenance window (23:50-00:20 GMT+8), using cached data");
            
            if cached_data.is_empty() {
                info!("No cached data available, attempting to fetch fresh data anyway");
            } else {
                let data_points: Vec<DataPoint> = cached_data.values()
                    .map(|area| create_data_point(area))
                    .collect();
                
                info!("Using cached data for {} areas", data_points.len());
                
                for area in cached_data.values() {
                    info!("Cached - Area {}: {} free spaces", area.area_code, area.area_free_space_num);
                }
                
                match write_points(&client, &config.influxdb.bucket, data_points).await {
                        Ok(_) => info!("Successfully wrote cached data to InfluxDB"),
                        Err(e) => error!("Failed to write cached data to InfluxDB: {:#}", e),
                    }
                
                continue;
        }
    }
        
        match fetch_parking_data(&http, &config.api.url).await {
            Ok(data) => {
                if !data.success {
                    error!("API returned unsuccessful response");
                    continue;
                }
                
                if data.msparking_data.is_empty() {
                    error!("No parking data available in the response");
                    continue;
                }
                
                update_cache(&mut cached_data, &data.msparking_data);
                
                let data_points: Vec<DataPoint> = data.msparking_data
                    .iter()
                    .map(create_data_point)
                    .collect();
                
                info!("Found parking data for {} areas", data_points.len());
                
                for area in &data.msparking_data {
                    info!("Area {}: {} free spaces", area.area_code, area.area_free_space_num);
                }
                
                match write_points(&client, &config.influxdb.bucket, data_points).await {
                        Ok(_) => info!("Successfully wrote data to InfluxDB"),
                        Err(e) => error!("Failed to write to InfluxDB: {:#}", e),
                    }
            }
            Err(e) => {
                error!("Error fetching parking data: {:#}", e);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    
    let config = load_config().await?;
    info!("Configuration loaded successfully");
    
    run_scraper(config).await?;
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_areas() {
        assert_eq!(location_for(12), "SIP-B25-B26");
        assert_eq!(location_for(2), "ZHONGMENG");
        assert_eq!(location_for(4), "SIP-B3");
        assert_eq!(location_for(18), "SIP-B3");
        assert_eq!(location_for(6), "SIP-B5");
        assert_eq!(location_for(19), "SIP-B5");
        assert_eq!(location_for(99), "Unknown");
    }

    #[test]
    fn parses_live_payload_with_type_field() {
        let raw = r#"{"success":true,"msparkingData":[{"type":1,"areaCode":12,"areaFreeSpaceNum":54},{"type":3,"areaCode":2,"areaFreeSpaceNum":117},{"type":5,"areaCode":18,"areaFreeSpaceNum":17},{"type":5,"areaCode":19,"areaFreeSpaceNum":17}],"date":"2026.10.29    星期四"}"#;
        let r: ApiResponse = serde_json::from_str(raw).unwrap();
        let locs: Vec<_> = r.msparking_data.iter().map(|a| location_for(a.area_code)).collect();
        assert_eq!(locs, ["SIP-B25-B26", "ZHONGMENG", "SIP-B3", "SIP-B5"]);
    }

    fn area(code: i32, n: i64) -> AreaData {
        AreaData { area_code: code, area_free_space_num: n }
    }

    #[test]
    fn cache_drops_retired_codes_and_keeps_last_nonzero() {
        let mut cache = HashMap::new();
        update_cache(&mut cache, &[area(12, 30), area(4, 8), area(6, 10)]);
        update_cache(&mut cache, &[area(12, 0), area(18, 17), area(19, 17)]);
        let mut keys: Vec<_> = cache.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, [12, 18, 19]);
        assert_eq!(cache[&12].area_free_space_num, 30); // zero doesn't overwrite last non-zero
    }
}