use anyhow::{Context, Result};
use chrono::Utc;
use config::{Config, File};
use futures::stream;
use influxdb2::Client;
use influxdb2::models::DataPoint;
use log::{error, info};
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Deserialize, Serialize)]
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

fn create_data_point(area: &AreaData) -> DataPoint {
    let now = Utc::now();
    
    DataPoint::builder("parking_spaces")
        .tag("area_code", area.area_code.to_string())
        .field("free_spaces", area.area_free_space_num)
        .timestamp(now.timestamp_nanos_opt().unwrap())
        .build()
        .unwrap()
}

async fn run_scraper(config: AppConfig) -> Result<()> {
    let client = Arc::new(Client::new(&config.influxdb.url, &config.influxdb.org, &config.influxdb.token));
    
    let mut interval = time::interval(Duration::from_secs(config.api.scraping_interval_secs));
    
    info!("Starting parking data scraper. Interval: {} seconds", config.api.scraping_interval_secs);
    
    loop {
        interval.tick().await;
        info!("Fetching parking data...");
        
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