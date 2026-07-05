//! Temperature reading abstraction.
//!
//! Provides a [`TemperatureSource`] trait with two implementations:
//! [`OpenMeteo`] (fetches live data from the Open-Meteo API) and
//! [`StaticTemp`] (returns a fixed value).  A factory selects the impl
//! from [`crate::config::TempCfg`].

use std::time::Duration;

use serde::Deserialize;

use crate::config::TempCfg;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over a temperature source.
#[async_trait::async_trait]
pub trait TemperatureSource: Send + Sync {
    /// Return the current temperature in °C, or `None` on failure.
    async fn read(&self) -> Option<f64>;
}

// ---------------------------------------------------------------------------
// Open-Meteo implementation
// ---------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

struct CacheEntry {
    temp: f64,
    fetched_at: tokio::time::Instant,
}

/// Reads live temperature from [Open-Meteo](https://open-meteo.com/).
///
/// Results are cached for 5 minutes so the monitoring loop doesn't hammer
/// the free API.
pub struct OpenMeteo {
    lat: f64,
    lon: f64,
    cache: tokio::sync::Mutex<Option<CacheEntry>>,
}

impl OpenMeteo {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self {
            lat,
            lon,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    /// Build the API URL for the configured coordinates.
    fn url(&self) -> String {
        format!(
            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m",
            self.lat, self.lon
        )
    }
}

#[derive(Deserialize)]
struct OpenMeteoResponse {
    current: CurrentData,
}

#[derive(Deserialize)]
struct CurrentData {
    #[serde(rename = "temperature_2m")]
    temperature_2m: f64,
}

#[async_trait::async_trait]
impl TemperatureSource for OpenMeteo {
    async fn read(&self) -> Option<f64> {
        // Check cache first.
        {
            let guard = self.cache.lock().await;
            if let Some(entry) = guard.as_ref() {
                if entry.fetched_at.elapsed() < CACHE_TTL {
                    return Some(entry.temp);
                }
            }
        } // drop lock before the HTTP call

        // Fetch fresh data.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;

        let url = self.url();
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[linewatch] OpenMeteo HTTP error: {e}");
                return None;
            }
        };

        let parsed: OpenMeteoResponse = match resp.json().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[linewatch] OpenMeteo parse error: {e}");
                return None;
            }
        };
        let temp = parsed.current.temperature_2m;

        // Update cache.
        let mut guard = self.cache.lock().await;
        *guard = Some(CacheEntry {
            temp,
            fetched_at: tokio::time::Instant::now(),
        });

        Some(temp)
    }
}

// ---------------------------------------------------------------------------
// Static temperature implementation
// ---------------------------------------------------------------------------

/// Returns a fixed temperature value.  Used when the user provides
/// `static_c` in the config.
pub struct StaticTemp(pub f64);

#[async_trait::async_trait]
impl TemperatureSource for StaticTemp {
    async fn read(&self) -> Option<f64> {
        Some(self.0)
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a [`TemperatureSource`] from the temperature section of the config.
///
/// * If `cfg.source == "static"` and `static_c` is `Some`, returns a
///   [`StaticTemp`].
/// * Otherwise returns an [`OpenMeteo`] instance (which may later fail
///   gracefully at read time if the network is unavailable).
pub fn create_temperature_source(cfg: &TempCfg) -> Box<dyn TemperatureSource> {
    if cfg.source == "static" {
        if let Some(c) = cfg.static_c {
            return Box::new(StaticTemp(c));
        }
    }

    Box::new(OpenMeteo::new(cfg.lat, cfg.lon))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_meteo_live() {
        // Use real coordinates for Rome, Italy.
        let src = OpenMeteo::new(41.8931, 12.4828);
        let temp = src.read().await;
        println!("Open-Meteo live temperature: {:?}°C", temp);
        // Print diagnostics even on failure.
        if let Some(t) = temp {
            assert!(
                t > -50.0 && t < 60.0,
                "temperature out of plausible range: {t}"
            );
        }
    }

    #[tokio::test]
    async fn open_meteo_caches() {
        let src = OpenMeteo::new(41.8931, 12.4828);

        // First call – fetches from network.
        let t1 = src.read().await;
        if t1.is_none() {
            eprintln!("open_meteo_caches: first call returned None, skipping cache test");
            return;
        }

        // Second call – should hit cache (same Instant, definitely < 5 min).
        let t2 = src.read().await;
        assert!(t2.is_some());
        assert_eq!(t1, t2, "cached value should match");
    }

    #[tokio::test]
    async fn static_temp_returns_fixed_value() {
        let src = StaticTemp(23.5);
        let temp = src.read().await;
        assert_eq!(temp, Some(23.5));
    }

    #[tokio::test]
    async fn factory_static() {
        let cfg = TempCfg {
            source: "static".into(),
            lat: 0.0,
            lon: 0.0,
            static_c: Some(18.0),
        };
        let src = create_temperature_source(&cfg);
        let temp = src.read().await;
        assert_eq!(temp, Some(18.0));
    }

    #[tokio::test]
    async fn factory_open_meteo_without_static() {
        let cfg = TempCfg {
            source: "open-meteo".into(),
            lat: 41.8931,
            lon: 12.4828,
            static_c: None,
        };
        let src = create_temperature_source(&cfg);
        let temp = src.read().await;
        println!("Factory Open-Meteo temperature: {:?}°C", temp);
        // The API may fail in test environments, but the factory shouldn't panic.
        if let Some(t) = temp {
            assert!(t > -50.0 && t < 60.0, "unplausible temperature: {t}");
        }
    }

    #[tokio::test]
    async fn factory_static_fallback_to_open_meteo_when_no_static_c() {
        // source=static but without static_c → fall back to Open-Meteo
        let cfg = TempCfg {
            source: "static".into(),
            lat: 41.8931,
            lon: 12.4828,
            static_c: None,
        };
        let src = create_temperature_source(&cfg);
        let temp = src.read().await;
        // Should not panic; may return None if network unavailable.
        println!("Fallback temperature: {:?}°C", temp);
    }
}
