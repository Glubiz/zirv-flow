//! The egress policy: which destination addresses outbound connections may reach.
//!
//! This is the pure half of the SSRF defence. It classifies an IP address, matches the
//! operator's allowlist and returns a decision with the reason. Each outbound crate enforces it
//! at its own connect point, after name resolution, and dials only the addresses it checked.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::IpNet;

/// What kind of destination an address is. Only [`AddressClass::Public`] is reachable without an
/// allowlist entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AddressClass {
    Public,
    Loopback,
    /// RFC 1918 and the deprecated IPv6 site-local range.
    Private,
    /// IPv6 `fc00::/7`.
    UniqueLocal,
    LinkLocal,
    /// RFC 6598 carrier-grade NAT, `100.64.0.0/10`.
    SharedCgnat,
    /// A cloud instance-metadata endpoint.
    Metadata,
    Unspecified,
    Multicast,
    Broadcast,
    Documentation,
    Benchmarking,
    /// Everything else that is not global unicast, including Teredo.
    Reserved,
}

impl AddressClass {
    /// A stable lowercase name, used in messages and as a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::UniqueLocal => "unique_local",
            Self::LinkLocal => "link_local",
            Self::SharedCgnat => "shared_cgnat",
            Self::Metadata => "metadata",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Broadcast => "broadcast",
            Self::Documentation => "documentation",
            Self::Benchmarking => "benchmarking",
            Self::Reserved => "reserved",
        }
    }
}

impl fmt::Display for AddressClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Cloud metadata endpoints: AWS and GCP/Azure, ECS task credentials, Alibaba, and AWS over IPv6.
const METADATA_V4: [Ipv4Addr; 4] = [
    Ipv4Addr::new(169, 254, 169, 254),
    Ipv4Addr::new(169, 254, 170, 2),
    Ipv4Addr::new(100, 100, 100, 200),
    // The Azure WireServer.
    Ipv4Addr::new(168, 63, 129, 16),
];
const METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

/// The IPv4 address an IPv6 address embeds, when it is one of the forms that carry one:
/// IPv4-mapped, IPv4-compatible, NAT64 (both well-known prefixes) and 6to4.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = ip.octets();
    let segments = ip.segments();
    let low = Ipv4Addr::new(o[12], o[13], o[14], o[15]);
    if segments[..5] == [0; 5] && segments[5] == 0xffff {
        return Some(low);
    }
    // `::/96`, except `::` and `::1`, which are their own classes.
    if segments[..6] == [0; 6] && !ip.is_unspecified() && !ip.is_loopback() {
        return Some(low);
    }
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(low);
    }
    // RFC 8215 local-use prefix; RFC 6052 section 2.2 places the address after the /48, around
    // the reserved `u` octet.
    if segments[..3] == [0x64, 0xff9b, 1] {
        return Some(Ipv4Addr::new(o[6], o[7], o[9], o[10]));
    }
    if segments[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

fn classify_v4(ip: Ipv4Addr) -> AddressClass {
    if METADATA_V4.contains(&ip) {
        return AddressClass::Metadata;
    }
    let [a, b, c, _] = ip.octets();
    match (a, b, c) {
        (0, ..) => AddressClass::Unspecified,
        (127, ..) => AddressClass::Loopback,
        (10, ..) => AddressClass::Private,
        (172, 16..=31, _) => AddressClass::Private,
        (192, 168, _) => AddressClass::Private,
        (169, 254, _) => AddressClass::LinkLocal,
        (100, 64..=127, _) => AddressClass::SharedCgnat,
        (192, 0, 2) | (198, 51, 100) | (203, 0, 113) => AddressClass::Documentation,
        (198, 18..=19, _) => AddressClass::Benchmarking,
        // IETF protocol assignments and the retired 6to4 relay anycast block.
        (192, 0, 0) | (192, 88, 99) => AddressClass::Reserved,
        (224..=239, ..) => AddressClass::Multicast,
        (255, ..) if ip == Ipv4Addr::BROADCAST => AddressClass::Broadcast,
        (240..=255, ..) => AddressClass::Reserved,
        _ => AddressClass::Public,
    }
}

fn classify_v6(ip: Ipv6Addr) -> AddressClass {
    if let Some(v4) = embedded_v4(ip) {
        return classify_v4(v4);
    }
    if ip == METADATA_V6 {
        return AddressClass::Metadata;
    }
    if ip.is_unspecified() {
        return AddressClass::Unspecified;
    }
    if ip.is_loopback() {
        return AddressClass::Loopback;
    }
    let s = ip.segments();
    match s[0] {
        0xfc00..=0xfdff => AddressClass::UniqueLocal,
        0xfe80..=0xfebf => AddressClass::LinkLocal,
        0xfec0..=0xfeff => AddressClass::Private,
        0xff00..=0xffff => AddressClass::Multicast,
        // Teredo tunnels, whose embedded address is obfuscated.
        0x2001 if s[1] == 0 => AddressClass::Reserved,
        0x2001 if s[1] == 0x0db8 => AddressClass::Documentation,
        0x2001 if s[1] == 0x0002 && s[2] == 0 => AddressClass::Benchmarking,
        0x3fff if s[1] <= 0x0fff => AddressClass::Documentation,
        // Global unicast is `2000::/3`; the rest of the space is unallocated or special.
        0x2000..=0x3fff => AddressClass::Public,
        _ => AddressClass::Reserved,
    }
}

/// Classifies an address. IPv6 forms that embed an IPv4 address are classified by that address.
pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

/// Whether an address may be connected to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    /// Refused, with the class that caused it.
    Deny(AddressClass),
}

/// An allowlist entry that could not be parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressConfigError {
    pub entry: String,
    pub reason: String,
}

impl fmt::Display for EgressConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.entry, self.reason)
    }
}

impl std::error::Error for EgressConfigError {}

/// The operator's allowlist of CIDR ranges that outbound connections may reach even though their
/// class is not public.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EgressPolicy {
    allow: Vec<IpNet>,
}

impl EgressPolicy {
    /// Parses `egress.allow`. Every entry must be a CIDR range such as `10.0.5.7/32`; a bare
    /// address or an entry with host bits set is refused rather than guessed at.
    pub fn parse(entries: &[String]) -> Result<Self, EgressConfigError> {
        let mut allow = Vec::with_capacity(entries.len());
        for entry in entries {
            let fail = |reason: String| EgressConfigError {
                entry: entry.clone(),
                reason,
            };
            let net: IpNet = entry.trim().parse().map_err(|_| {
                fail("is not a CIDR range such as 10.0.0.0/8 or 10.0.5.7/32".to_owned())
            })?;
            if net.addr() != net.network() {
                return Err(fail(format!(
                    "has host bits set; write it as {}",
                    net.trunc()
                )));
            }
            allow.push(net);
        }
        Ok(Self { allow })
    }

    /// Whether the allowlist unlocks `ip`. A metadata address needs an entry naming exactly it.
    fn allows(&self, ip: IpAddr, class: AddressClass) -> bool {
        let embedded = match ip {
            IpAddr::V6(v6) => embedded_v4(v6).map(IpAddr::V4),
            IpAddr::V4(_) => None,
        };
        std::iter::once(ip).chain(embedded).any(|candidate| {
            self.allow.iter().any(|net| {
                if class == AddressClass::Metadata {
                    net.prefix_len() == net.max_prefix_len() && net.addr() == candidate
                } else {
                    net.contains(&candidate)
                }
            })
        })
    }

    pub fn decide(&self, ip: IpAddr) -> Decision {
        let class = classify(ip);
        if class == AddressClass::Public || self.allows(ip, class) {
            Decision::Allow
        } else {
            Decision::Deny(class)
        }
    }

    /// Decides a resolved set: if any address is denied the whole set is, with the first denial's
    /// class. A host resolving to a mix is refused, never silently filtered.
    pub fn decide_all(&self, ips: &[IpAddr]) -> Decision {
        ips.iter()
            .map(|ip| self.decide(*ip))
            .find(|decision| matches!(decision, Decision::Deny(_)))
            .unwrap_or(Decision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::AddressClass::*;
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn policy(entries: &[&str]) -> EgressPolicy {
        let owned: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
        EgressPolicy::parse(&owned).unwrap()
    }

    #[test]
    fn every_class_at_its_first_and_last_address() {
        let cases: &[(AddressClass, &[&str])] = &[
            (
                Public,
                &[
                    "1.0.0.0",
                    "8.8.8.8",
                    "100.63.255.255",
                    "100.128.0.0",
                    "126.255.255.255",
                    "128.0.0.0",
                    "172.15.255.255",
                    "172.32.0.0",
                    "191.255.255.255",
                    "192.0.1.255",
                    "192.0.3.0",
                    "192.167.255.255",
                    "192.169.0.0",
                    "198.17.255.255",
                    "198.20.0.0",
                    "223.255.255.255",
                    "2000::",
                    "2606:4700::1111",
                    "3ffe:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (
                Loopback,
                &["127.0.0.0", "127.0.0.1", "127.255.255.255", "::1"],
            ),
            (
                Private,
                &[
                    "10.0.0.0",
                    "10.255.255.255",
                    "172.16.0.0",
                    "172.31.255.255",
                    "192.168.0.0",
                    "192.168.255.255",
                    "fec0::",
                    "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (
                UniqueLocal,
                &[
                    "fc00::",
                    "fd00::1",
                    "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (
                LinkLocal,
                &[
                    "169.254.0.0",
                    "169.254.255.255",
                    "fe80::",
                    "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (SharedCgnat, &["100.64.0.0", "100.127.255.255"]),
            (
                Metadata,
                &[
                    "169.254.169.254",
                    "169.254.170.2",
                    "100.100.100.200",
                    "168.63.129.16",
                    "fd00:ec2::254",
                ],
            ),
            (Unspecified, &["0.0.0.0", "0.255.255.255", "::"]),
            (
                Multicast,
                &[
                    "224.0.0.0",
                    "239.255.255.255",
                    "ff00::",
                    "ff02::1",
                    "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (Broadcast, &["255.255.255.255"]),
            (
                Documentation,
                &[
                    "192.0.2.0",
                    "192.0.2.255",
                    "198.51.100.0",
                    "198.51.100.255",
                    "203.0.113.0",
                    "203.0.113.255",
                    "2001:db8::",
                    "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
                    "3fff::",
                    "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (
                Benchmarking,
                &[
                    "198.18.0.0",
                    "198.19.255.255",
                    "2001:2::",
                    "2001:2:0:ffff:ffff:ffff:ffff:ffff",
                ],
            ),
            (
                Reserved,
                &[
                    "240.0.0.0",
                    "255.255.255.254",
                    "192.0.0.0",
                    "192.0.0.255",
                    "192.88.99.1",
                    "2001::1",
                    "2001:0:ffff:ffff:ffff:ffff:ffff:ffff",
                    "100::1",
                    "4000::",
                    "8000::1",
                    "f000::1",
                ],
            ),
        ];
        for (class, addresses) in cases {
            for address in *addresses {
                assert_eq!(classify(ip(address)), *class, "{address}");
            }
        }
    }

    #[test]
    fn embedded_ipv4_forms_take_the_class_of_the_embedded_address() {
        let cases = [
            // IPv4-mapped.
            ("::ffff:127.0.0.1", Loopback),
            ("::ffff:10.1.2.3", Private),
            ("::ffff:169.254.169.254", Metadata),
            ("::ffff:8.8.8.8", Public),
            ("::ffff:0:0", Unspecified),
            // IPv4-compatible, other than `::` and `::1`.
            ("::7f00:1", Loopback),
            ("::a00:1", Private),
            ("::808:808", Public),
            // NAT64, well-known prefix.
            ("64:ff9b::7f00:1", Loopback),
            ("64:ff9b::a9fe:a9fe", Metadata),
            ("64:ff9b::808:808", Public),
            // 6to4.
            ("2002:7f00:1::", Loopback),
            ("2002:c0a8:101::1", Private),
            ("2002:808:808::", Public),
        ];
        for (address, class) in cases {
            assert_eq!(classify(ip(address)), class, "{address}");
        }
    }

    #[test]
    fn local_use_nat64_skips_the_reserved_octet() {
        // 64:ff9b:1::/48 with the address laid out per RFC 6052: two octets, `u`, two octets.
        assert_eq!(classify(ip("64:ff9b:1:7f00:0:100::")), Loopback);
        assert_eq!(classify(ip("64:ff9b:1:a9fe:a9:fe00::")), Metadata);
        assert_eq!(classify(ip("64:ff9b:1:808:8:800::")), Public);
    }

    #[test]
    fn teredo_is_reserved_not_unwrapped() {
        let teredo = ip("2001:0:4136:e378:8000:63bf:3fff:fdd2");
        assert_eq!(classify(teredo), Reserved);
        assert_eq!(policy(&[]).decide(teredo), Decision::Deny(Reserved));
    }

    #[test]
    fn only_public_is_allowed_without_an_allowlist() {
        let none = policy(&[]);
        assert_eq!(none.decide(ip("8.8.8.8")), Decision::Allow);
        assert_eq!(none.decide(ip("2606:4700::1111")), Decision::Allow);
        assert_eq!(none.decide(ip("127.0.0.1")), Decision::Deny(Loopback));
        assert_eq!(none.decide(ip("::1")), Decision::Deny(Loopback));
        assert_eq!(
            none.decide(ip("::ffff:127.0.0.1")),
            Decision::Deny(Loopback)
        );
        assert_eq!(none.decide(ip("10.0.0.1")), Decision::Deny(Private));
        assert_eq!(none.decide(ip("169.254.169.254")), Decision::Deny(Metadata));
    }

    #[test]
    fn an_allowlist_entry_unlocks_only_the_addresses_it_contains() {
        let allow = policy(&["10.0.5.7/32", "192.168.0.0/16", "fd00::/16"]);
        assert_eq!(allow.decide(ip("10.0.5.7")), Decision::Allow);
        assert_eq!(allow.decide(ip("10.0.5.8")), Decision::Deny(Private));
        assert_eq!(allow.decide(ip("192.168.200.9")), Decision::Allow);
        assert_eq!(allow.decide(ip("172.16.0.1")), Decision::Deny(Private));
        assert_eq!(allow.decide(ip("fd12::1")), Decision::Deny(UniqueLocal));
        assert_eq!(allow.decide(ip("fd00::1")), Decision::Allow);
        assert_eq!(allow.decide(ip("fc00::1")), Decision::Deny(UniqueLocal));
        // The allowlist does not cross address families.
        assert_eq!(allow.decide(ip("127.0.0.1")), Decision::Deny(Loopback));
    }

    #[test]
    fn an_allowlisted_ipv4_range_also_unlocks_its_embedded_forms() {
        let allow = policy(&["127.0.0.1/32"]);
        assert_eq!(allow.decide(ip("::ffff:127.0.0.1")), Decision::Allow);
        assert_eq!(
            allow.decide(ip("::ffff:127.0.0.2")),
            Decision::Deny(Loopback)
        );
    }

    #[test]
    fn a_broad_range_never_unlocks_metadata_but_the_exact_address_does() {
        let broad = policy(&[
            "169.254.0.0/16",
            "100.64.0.0/10",
            "168.63.0.0/16",
            "0.0.0.0/0",
            "fd00::/8",
        ]);
        assert_eq!(
            broad.decide(ip("169.254.169.254")),
            Decision::Deny(Metadata)
        );
        assert_eq!(
            broad.decide(ip("100.100.100.200")),
            Decision::Deny(Metadata)
        );
        assert_eq!(broad.decide(ip("168.63.129.16")), Decision::Deny(Metadata));
        assert_eq!(broad.decide(ip("fd00:ec2::254")), Decision::Deny(Metadata));
        assert_eq!(
            broad.decide(ip("::ffff:169.254.169.254")),
            Decision::Deny(Metadata)
        );
        // The broad range still unlocks the non-metadata addresses it covers.
        assert_eq!(broad.decide(ip("169.254.1.1")), Decision::Allow);

        let exact = policy(&["169.254.169.254/32", "fd00:ec2::254/128"]);
        assert_eq!(exact.decide(ip("169.254.169.254")), Decision::Allow);
        assert_eq!(exact.decide(ip("169.254.170.2")), Decision::Deny(Metadata));
        assert_eq!(exact.decide(ip("fd00:ec2::254")), Decision::Allow);
        assert_eq!(exact.decide(ip("168.63.129.16")), Decision::Deny(Metadata));
        let wire = policy(&["168.63.129.16/32"]);
        assert_eq!(wire.decide(ip("168.63.129.16")), Decision::Allow);
        assert_eq!(exact.decide(ip("169.254.1.1")), Decision::Deny(LinkLocal));
    }

    #[test]
    fn a_mixed_set_is_denied_as_a_whole() {
        let none = policy(&[]);
        assert_eq!(
            none.decide_all(&[ip("8.8.8.8"), ip("1.1.1.1")]),
            Decision::Allow
        );
        assert_eq!(none.decide_all(&[]), Decision::Allow);
        assert_eq!(
            none.decide_all(&[ip("8.8.8.8"), ip("127.0.0.1")]),
            Decision::Deny(Loopback)
        );
        assert_eq!(
            none.decide_all(&[ip("127.0.0.1"), ip("8.8.8.8")]),
            Decision::Deny(Loopback)
        );
        let loopback = policy(&["127.0.0.1/32"]);
        assert_eq!(
            loopback.decide_all(&[ip("8.8.8.8"), ip("127.0.0.1")]),
            Decision::Allow
        );
        assert_eq!(
            loopback.decide_all(&[ip("127.0.0.1"), ip("10.0.0.1")]),
            Decision::Deny(Private)
        );
    }

    #[test]
    fn parse_accepts_cidr_ranges_and_an_empty_list() {
        assert_eq!(EgressPolicy::parse(&[]).unwrap(), EgressPolicy::default());
        policy(&[
            "10.0.0.0/8",
            " 192.168.1.0/24 ",
            "::1/128",
            "fd00::/8",
            "0.0.0.0/0",
        ]);
    }

    #[test]
    fn parse_refuses_bad_entries_naming_the_entry_and_the_reason() {
        let cases = [
            ("10.0.5.7", "is not a CIDR range"),
            ("localhost", "is not a CIDR range"),
            ("10.0.0.0/33", "is not a CIDR range"),
            ("::1/129", "is not a CIDR range"),
            ("", "is not a CIDR range"),
            ("10.0.0.5/8", "has host bits set; write it as 10.0.0.0/8"),
        ];
        for (entry, reason) in cases {
            let error = EgressPolicy::parse(&[entry.to_owned()]).unwrap_err();
            assert_eq!(error.entry, entry);
            assert!(
                error.reason.starts_with(reason),
                "{entry}: {}",
                error.reason
            );
            assert!(error.to_string().contains(entry));
        }
        let error = EgressPolicy::parse(&["10.0.0.0/8".to_owned(), "nope".to_owned()]).unwrap_err();
        assert_eq!(error.entry, "nope");
    }

    #[test]
    fn class_names_are_stable_labels() {
        assert_eq!(UniqueLocal.as_str(), "unique_local");
        assert_eq!(Metadata.to_string(), "metadata");
    }
}
