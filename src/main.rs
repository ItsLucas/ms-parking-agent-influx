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

async fn fetch_parking_data(url: &str) -> Result<ApiResponse> {
    let response = reqwest::get(url)
        .await
        .context("Failed to send request")?;
    
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

fn create_data_point(area: &AreaData) -> DataPoint {
    let now = Utc::now();
    
    DataPoint::builder("parking_spaces")
        .tag("area_code", area.area_code.to_string())
        .tag("location", match area.area_code {
            12 => "SIP-B25-B26".to_string(),
            2 => "ZHONGMENG".to_string(),
            _ => "Unknown".to_string(),
        })
        .field("free_spaces", area.area_free_space_num)
        .timestamp(now.timestamp_nanos_opt().unwrap())
        .build()
        .unwrap()
}

async fn run_scraper(config: AppConfig) -> Result<()> {
    let client = Arc::new(Client::new(&config.influxdb.url, &config.influxdb.org, &config.influxdb.token));
    
    let mut interval = time::interval(Duration::from_secs(config.api.scraping_interval_secs));
    
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
                
                match client.write(&config.influxdb.bucket, stream::iter(data_points))
                    .await {
                        Ok(_) => info!("Successfully wrote cached data to InfluxDB"),
                        Err(e) => error!("Failed to write cached data to InfluxDB: {}", e),
                    }
                
                continue;
        }
    }
        
        match fetch_parking_data(&config.api.url).await {
            Ok(data) => {
                if !data.success {
                    error!("API returned unsuccessful response");
                    continue;
                }
                
                if data.msparking_data.is_empty() {
                    error!("No parking data available in the response");
                    continue;
                }
                
                for area in &data.msparking_data {
                    if area.area_free_space_num > 0 {
                        cached_data.insert(area.area_code, area.clone());
                    }
                }
                
                let data_points: Vec<DataPoint> = data.msparking_data
                    .iter()
                    .map(create_data_point)
                    .collect();
                
                info!("Found parking data for {} areas", data_points.len());
                
                for area in &data.msparking_data {
                    info!("Area {}: {} free spaces", area.area_code, area.area_free_space_num);
                }
                
                match client.write(&config.influxdb.bucket, stream::iter(data_points))
                    .await {
                        Ok(_) => info!("Successfully wrote data to InfluxDB"),
                        Err(e) => error!("Failed to write to InfluxDB: {}", e),
                    }
            }
            Err(e) => {
                error!("Error fetching parking data: {}", e);
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