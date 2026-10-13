//! Deterministic payloads. Item `n` of a run depends only on the seed and `n`, so a run can be
//! repeated exactly; every item carries a `messageId` that is unique within a run id.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Identify,
    Track,
}

/// SplitMix64: tiny, fast and good enough for synthetic data.
struct Rng(u64);

impl Rng {
    fn for_item(seed: u64, seq: u64) -> Self {
        let mut rng = Rng(seed ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        rng.next();
        rng
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }
}

const PLANS: [&str; 4] = ["free", "pro", "team", "enterprise"];
const EVENTS: [&str; 5] = [
    "bench_page_viewed",
    "bench_item_added",
    "bench_order_placed",
    "bench_plan_changed",
    "bench_support_opened",
];

/// What item `seq` of a batch-mode run is: half identifies, half tracks, by the seed.
pub fn batch_kind(seed: u64, seq: u64) -> Kind {
    if Rng::for_item(seed, seq ^ 0xB47C_4000).below(2) == 0 {
        Kind::Identify
    } else {
        Kind::Track
    }
}

/// The JSON of item `seq`. `typed` adds the `type` field a batch item needs. No item carries
/// a `timestamp`, so the server stamps its arrival time (which the applied-latency SQL uses).
pub fn item(seed: u64, run_id: &str, people: u64, seq: u64, kind: Kind, typed: bool) -> Value {
    let mut rng = Rng::for_item(seed, seq);
    let person = rng.below(people);
    let mut value = match kind {
        Kind::Identify => json!({
            "userId": format!("bench-u-{person}"),
            "traits": {
                "plan": PLANS[rng.below(PLANS.len() as u64) as usize],
                "seats": rng.below(500),
                "score": rng.below(10_000) as f64 / 100.0,
                "bench_seq": seq,
            },
            "messageId": format!("lg-{run_id}-{seq}"),
        }),
        Kind::Track => json!({
            "userId": format!("bench-u-{person}"),
            "event": EVENTS[rng.below(EVENTS.len() as u64) as usize],
            "properties": {
                "amount": rng.below(100_000) as f64 / 100.0,
                "quantity": rng.below(20),
                "bench_seq": seq,
            },
            "messageId": format!("lg-{run_id}-{seq}"),
        }),
    };
    if typed {
        let name = match kind {
            Kind::Identify => "identify",
            Kind::Track => "track",
        };
        value["type"] = Value::from(name);
    }
    value
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use super::*;

    #[test]
    fn an_item_depends_only_on_the_seed_and_its_number() {
        let a = item(7, "r", 1000, 42, Kind::Track, false);
        assert_eq!(a, item(7, "r", 1000, 42, Kind::Track, false));
        assert_ne!(a, item(8, "r", 1000, 42, Kind::Track, false));
        assert_ne!(a, item(7, "r", 1000, 43, Kind::Track, false));
    }

    #[test]
    fn message_ids_are_unique_across_a_run_and_differ_between_run_ids() {
        let ids: HashSet<String> = (0..20_000)
            .map(|seq| {
                item(1, "run-a", 50, seq, Kind::Identify, false)["messageId"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(ids.len(), 20_000);
        let other = item(1, "run-b", 50, 0, Kind::Identify, false);
        assert!(!ids.contains(other["messageId"].as_str().unwrap()));
    }

    #[test]
    fn people_stay_inside_the_pool_and_the_kinds_have_their_fields() {
        for seq in 0..2000 {
            let identify = item(3, "r", 10, seq, Kind::Identify, false);
            let user = identify["userId"].as_str().unwrap();
            let n: u64 = user.strip_prefix("bench-u-").unwrap().parse().unwrap();
            assert!(n < 10);
            assert!(identify["traits"].is_object() && identify.get("event").is_none());
            let track = item(3, "r", 10, seq, Kind::Track, true);
            assert!(track["event"].is_string() && track["properties"].is_object());
            assert_eq!(track["type"], "track");
        }
    }

    #[test]
    fn batch_mode_mixes_both_kinds() {
        let identifies = (0..1000)
            .filter(|seq| batch_kind(9, *seq) == Kind::Identify)
            .count();
        assert!((400..600).contains(&identifies), "{identifies}");
    }
}
