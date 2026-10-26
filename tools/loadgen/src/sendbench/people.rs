//! Seeds the people the send benchmark enrolls, through the product's ingest endpoint: one
//! `identify` per person with an email identifier, in batches. Consent and enrollment come
//! from the API afterwards (the scripts); nothing here touches the database.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Semaphore;

/// The address of person `n`. Lower case, on a reserved test domain.
pub(crate) fn address(prefix: &str, domain: &str, n: u64) -> String {
    format!("{prefix}-{n}@{domain}")
}

/// The body of one batch request: people `first..first + count`.
pub(crate) fn batch_body(prefix: &str, domain: &str, first: u64, count: u64) -> Value {
    let items: Vec<Value> = (first..first + count)
        .map(|n| {
            json!({
                "type": "identify",
                "identifiers": {"email": address(prefix, domain, n)},
                "traits": {"bench": "send"},
                "messageId": format!("{prefix}-{n}"),
            })
        })
        .collect();
    json!({ "batch": items })
}

/// The batch ranges that cover `count` people: `(first, size)`.
pub(crate) fn ranges(count: u64, batch_size: u64) -> Vec<(u64, u64)> {
    let size = batch_size.max(1);
    (0..count.div_ceil(size))
        .map(|i| (i * size, size.min(count - i * size)))
        .collect()
}

pub(crate) struct Seed {
    pub(crate) url: String,
    pub(crate) key: String,
    pub(crate) prefix: String,
    pub(crate) domain: String,
    pub(crate) count: u64,
    pub(crate) batch_size: u64,
    pub(crate) concurrency: usize,
}

/// Sends every batch; returns the number of people accepted.
pub(crate) async fn run(seed: Seed) -> Result<u64, String> {
    crate::init_tls();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let accepted = Arc::new(AtomicU64::new(0));
    let slots = Arc::new(Semaphore::new(seed.concurrency.max(1)));
    let mut tasks = Vec::new();
    for (first, size) in ranges(seed.count, seed.batch_size) {
        let permit = Arc::clone(&slots)
            .acquire_owned()
            .await
            .map_err(|error| error.to_string())?;
        let client = client.clone();
        let url = format!("{}/api/v1/ingest/batch", seed.url.trim_end_matches('/'));
        let key = seed.key.clone();
        let body = batch_body(&seed.prefix, &seed.domain, first, size).to_string();
        let accepted = Arc::clone(&accepted);
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            for _ in 0..8 {
                let response = client
                    .post(&url)
                    .bearer_auth(&key)
                    .header("content-type", "application/json")
                    .body(body.clone())
                    .send()
                    .await;
                match response {
                    Ok(r) if r.status().as_u16() == 202 => {
                        accepted.fetch_add(size, Ordering::Relaxed);
                        return Ok(());
                    }
                    Ok(r) if r.status().as_u16() == 429 => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Ok(r) => return Err(format!("batch at {first}: status {}", r.status())),
                    Err(error) => return Err(format!("batch at {first}: {error}")),
                }
            }
            Err(format!(
                "batch at {first}: still rate limited after 8 tries"
            ))
        }));
    }
    for task in tasks {
        task.await.map_err(|error| error.to_string())??;
    }
    Ok(accepted.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_deterministic_and_lower_case() {
        assert_eq!(address("bs1", "bench.example", 7), "bs1-7@bench.example");
    }

    #[test]
    fn a_batch_has_one_identify_per_person_with_an_email_identifier() {
        let body = batch_body("bs1", "bench.example", 10, 3);
        let items = body["batch"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "identify");
        assert_eq!(items[0]["identifiers"]["email"], "bs1-10@bench.example");
        assert_eq!(items[2]["messageId"], "bs1-12");
    }

    #[test]
    fn ranges_cover_every_person_once() {
        assert_eq!(ranges(0, 500), []);
        assert_eq!(ranges(1200, 500), [(0, 500), (500, 500), (1000, 200)]);
        let total: u64 = ranges(999_983, 500).iter().map(|(_, size)| size).sum();
        assert_eq!(total, 999_983);
    }
}
