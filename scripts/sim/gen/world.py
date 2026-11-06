"""The deterministic world of the simulated production environment (design section 2).

`person(n)` is a pure function of n, so every source and every generator derives the same
person from the same number. Mutable attributes (plan, loyalty points, billing status) start
here and drift later through gen-changes. Times are relative to an anchor, the instant of the
first seed, kept in /sim/seed/anchor.txt so every later process agrees on it.
"""

import os
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

MASK = (1 << 64) - 1
SEED_DIR = Path("/sim/seed")
PEOPLE = int(os.environ.get("SIM_PEOPLE", "100000"))

FIRST = ["Ada", "Ben", "Cara", "Dan", "Eva", "Finn", "Greta", "Hugo", "Ida", "Jens", "Karen",
         "Lars", "Mette", "Nils", "Olga", "Paul", "Rikke", "Sven", "Tina", "Ulf", "Vera",
         "William", "Xenia", "Yrsa", "Zoe", "Anders", "Bente", "Carl", "Dorte", "Erik", "Freja",
         "Gustav", "Hanne", "Ivan", "Julie", "Kasper", "Lone", "Mads", "Nanna", "Ole"]
LAST = ["Andersen", "Berg", "Christensen", "Dahl", "Eriksen", "Friis", "Gram", "Hansen",
        "Iversen", "Jensen", "Knudsen", "Larsen", "Madsen", "Nielsen", "Olsen", "Pedersen",
        "Rasmussen", "Sorensen", "Thomsen", "Ulrich", "Vestergaard", "Winther", "Moller",
        "Schmidt", "Weber", "Fischer", "Meyer", "Bakker", "Visser", "Smith", "Taylor",
        "Martin", "Bernard", "Garcia", "Lopez", "Nilsson", "Johansson", "Karlsson", "Lund", "Holm"]
COUNTRIES = [("DK", 35, ["Copenhagen", "Aarhus", "Odense", "Aalborg"]),
             ("DE", 15, ["Berlin", "Hamburg", "Munich"]),
             ("SE", 12, ["Stockholm", "Gothenburg", "Malmo"]),
             ("NO", 10, ["Oslo", "Bergen", "Trondheim"]),
             ("GB", 10, ["London", "Manchester", "Leeds"]),
             ("NL", 8, ["Amsterdam", "Rotterdam", "Utrecht"]),
             ("FR", 5, ["Paris", "Lyon", "Lille"]),
             ("ES", 5, ["Madrid", "Barcelona", "Valencia"])]
DOMAINS = ["example.com", "example.org", "example.net", "mail.example.com", "post.example.org"]
PLANS = [("free", 30), ("starter", 30), ("pro", 25), ("team", 10), ("enterprise", 5)]
MRR = {"free": 0, "starter": 19, "pro": 49, "team": 149, "enterprise": 499}
TIERS = [("bronze", 55), ("silver", 28), ("gold", 13), ("platinum", 4)]
METHODS = ["card", "card", "card", "invoice", "direct_debit", "paypal"]
LANGS = ["da", "da", "en", "en", "de", "sv", "nb", "nl", "fr", "es"]
BANDS = ["low", "mid", "mid", "high", "vip"]
RFM = ["champions", "loyal", "potential", "new", "at_risk", "hibernating", "lost"]
CATEGORIES = ["apparel", "home", "electronics", "grocery", "toys", "books", "sports", "beauty"]
CHANNELS = ["email", "chat", "phone", "web"]
PRIORITIES = ["low", "normal", "normal", "high", "urgent"]
FEATURES = ["export", "dashboard", "share", "search", "import", "reports", "api", "alerts"]
DEVICES = ["ios", "android", "desktop", "tablet"]


def mix(x):
    """splitmix64 finalizer: a cheap, well-spread hash of an integer."""
    x &= MASK
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK
    return x ^ (x >> 31)


def h(n, k):
    return mix(n * 64 + k + 1)


def pick_weighted(options, roll):
    """options: (value, weight, ...) tuples; roll in [0, total weight)."""
    for option in options:
        if roll < option[1]:
            return option
        roll -= option[1]
    return options[-1]


def anchor():
    """The seed instant (UTC), created on first use."""
    path = SEED_DIR / "anchor.txt"
    try:
        return datetime.fromisoformat(path.read_text().strip())
    except (OSError, ValueError):
        SEED_DIR.mkdir(parents=True, exist_ok=True)
        now = datetime.now(timezone.utc).replace(microsecond=0)
        path.write_text(now.isoformat())
        return now


def reset_anchor():
    (SEED_DIR / "anchor.txt").unlink(missing_ok=True)
    return anchor()


def customer_id(n):
    return f"C{n:07d}"


def email(n):
    return f"p{n}@{DOMAINS[h(n, 0) % len(DOMAINS)]}"


def ts(value):
    """ISO 8601 with a Z suffix, whole seconds."""
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sql_ts(value):
    return value.astimezone(timezone.utc).strftime("%Y-%m-%d %H:%M:%S")


def person(n, base=None):
    """Every stable attribute of person n (1-based). `base` is the anchor."""
    base = base or anchor()
    country, _, cities = pick_weighted(COUNTRIES, h(n, 1) % 100)
    plan = pick_weighted(PLANS, h(n, 2) % 100)[0]
    paying = plan != "free" and h(n, 3) % 100 < 85
    signup = base - timedelta(days=30 + h(n, 4) % 1500, seconds=h(n, 5) % 86400)
    orders = h(n, 6) % 12
    status = "closed" if h(n, 7) % 1000 < 12 else ("past_due" if h(n, 7) % 100 < 6 else "current")
    return {
        "n": n,
        "customer_id": customer_id(n),
        "email": email(n),
        "first_name": FIRST[h(n, 8) % len(FIRST)],
        "last_name": LAST[h(n, 9) % len(LAST)],
        "country": country,
        "city": cities[h(n, 10) % len(cities)],
        "signup_at": signup,
        "lifetime_orders": orders,
        "last_order_at": (base - timedelta(days=h(n, 11) % 400, seconds=h(n, 12) % 86400))
        if orders else None,
        "plan": plan,
        "plan_status": "active" if paying else ("trialing" if plan != "free" else "free"),
        "mrr": MRR[plan] if paying else 0,
        "renewal_at": base + timedelta(days=h(n, 13) % 30, seconds=h(n, 14) % 86400),
        "loyalty_tier": pick_weighted(TIERS, h(n, 15) % 100)[0],
        "loyalty_points": h(n, 16) % 5000,
        "billing_status": status,
        "balance_due": round((h(n, 17) % 20000) / 100, 2) if status == "past_due" else 0.0,
        "payment_method": METHODS[h(n, 18) % len(METHODS)],
        "last_payment_at": base - timedelta(days=h(n, 19) % 60, seconds=h(n, 20) % 86400),
        "newsletter_opt_in": h(n, 21) % 100 < 62,
        "ltv_band": BANDS[h(n, 22) % len(BANDS)],
        "preferred_language": LANGS[h(n, 23) % len(LANGS)],
        "rfm_segment": RFM[h(n, 24) % len(RFM)],
        "device": DEVICES[h(n, 25) % len(DEVICES)],
    }


def order_row(i, people, base):
    """Order i (1-based) of the 4-day history, spread evenly back from the anchor."""
    customer = h(i, 30) % people + 1
    placed = base - timedelta(seconds=(h(i, 31) % (4 * 86400)))
    return {
        "order_id": f"O{i:09d}",
        "customer_id": customer_id(customer),
        "amount": round(5 + (h(i, 32) % 45000) / 100, 2),
        "currency": "EUR" if h(i, 33) % 10 < 7 else "DKK",
        "category": CATEGORIES[h(i, 34) % len(CATEGORIES)],
        "items": 1 + h(i, 35) % 6,
        "placed_at": placed,
    }


def ticket_row(i, people, base):
    customer = h(i, 40) % people + 1
    return {
        "ticket_id": f"T{i:09d}",
        "customer_id": customer_id(customer),
        "priority": PRIORITIES[h(i, 41) % len(PRIORITIES)],
        "channel": CHANNELS[h(i, 42) % len(CHANNELS)],
        "status": "resolved" if h(i, 43) % 100 < 70 else "open",
        "opened_at": base - timedelta(seconds=(h(i, 44) % (4 * 86400))),
    }


def payment_row(i, people, base):
    customer = h(i, 50) % people + 1
    return {
        "payment_id": f"P{i:09d}",
        "customer_id": customer_id(customer),
        "amount": round(5 + (h(i, 51) % 30000) / 100, 2),
        "currency": "EUR",
        "received_at": base - timedelta(seconds=(h(i, 52) % (4 * 86400))),
    }


def kafka_value(i, people, base, window_seconds):
    """Backlog message i: the topic, the key, the JSON value and the message time."""
    customer = h(i, 60) % people + 1
    when = base - timedelta(seconds=h(i, 61) % window_seconds)
    if h(i, 62) % 100 < 80:
        topic = "app-feature-used"
        value = {"customer_id": customer_id(customer), "feature": FEATURES[h(i, 63) % len(FEATURES)],
                 "duration_ms": 50 + h(i, 64) % 120000, "at": ts(when)}
    else:
        topic = "app-license-activated"
        value = {"customer_id": customer_id(customer), "plan": pick_weighted(PLANS, h(i, 65) % 100)[0],
                 "seats": 1 + h(i, 66) % 50, "at": ts(when)}
    return topic, f"seed-{i:09d}", value, when


def now_ts():
    return ts(datetime.now(timezone.utc))


def wall():
    return time.time()
