//! Which addresses a probe may connect to.
//!
//! Every monitored target is on the public internet, so by default probes
//! refuse anything private, internal or special-purpose. That stops a monitor
//! from being used to reach the database, internal admin ports or a
//! platform's internal API (`fdaa::/16`, `_api.internal`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Address filter applied to resolved addresses, IP-literal URLs and every redirect hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddressPolicy {
    allow_non_public: bool,
}

impl AddressPolicy {
    /// Only globally routable unicast addresses (production).
    pub const PUBLIC_ONLY: Self = Self {
        allow_non_public: false,
    };

    /// Anything goes. For tests against local servers.
    pub const ALLOW_ALL: Self = Self {
        allow_non_public: true,
    };

    pub fn permits(self, ip: IpAddr) -> bool {
        self.allow_non_public || is_public(ip)
    }
}

impl Default for AddressPolicy {
    fn default() -> Self {
        Self::PUBLIC_ONLY
    }
}

/// Whether `ip` is a globally routable unicast address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let special = a == 0 // "this network"
        || a == 10 // RFC 1918
        || (a == 100 && (64..=127).contains(&b)) // CGNAT
        || a == 127 // loopback
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..=31).contains(&b)) // RFC 1918
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 168) // RFC 1918
        || (a == 198 && (18..=19).contains(&b)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224; // multicast, reserved, broadcast
    !special
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // IPv4 carried inside IPv6 is judged by the IPv4 address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [a, b] = segments[6].to_be_bytes();
        let [c, d] = segments[7].to_be_bytes();
        return is_public_v4(Ipv4Addr::new(a, b, c, d));
    }
    let first = segments[0];
    let special = ip.is_unspecified()
        || ip.is_loopback()
        || (first & 0xfe00) == 0xfc00 // unique local, includes platform-internal fdaa::/16
        || (first & 0xffc0) == 0xfe80 // link-local
        || (first & 0xff00) == 0xff00 // multicast
        || (first == 0x2001 && segments[1] == 0x0db8) // documentation
        || (first == 0x0100 && segments[1..4] == [0, 0, 0]) // discard-only
        || segments[..5] == [0, 0, 0, 0, 0]; // IPv4-compatible and other reserved ::/80
    !special
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case("1.1.1.1")]
    #[case("8.8.8.8")]
    #[case("151.101.1.140")]
    #[case("2606:4700:4700::1111")]
    #[case("2a00:1450:4001:80b::200e")]
    fn public_addresses_are_permitted(#[case] ip: &str) {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(is_public(ip), "{ip} should be public");
        assert!(AddressPolicy::PUBLIC_ONLY.permits(ip));
    }

    #[rstest]
    // IPv4 special-purpose ranges
    #[case("0.0.0.0")]
    #[case("0.1.2.3")] // "this network"
    #[case("10.0.0.1")] // RFC 1918
    #[case("172.16.0.1")]
    #[case("172.31.255.255")]
    #[case("192.168.1.1")]
    #[case("100.64.0.1")] // CGNAT
    #[case("100.127.255.255")]
    #[case("127.0.0.1")] // loopback
    #[case("169.254.169.254")] // link-local / cloud metadata
    #[case("192.0.0.1")] // IETF protocol assignments
    #[case("192.0.2.1")] // documentation
    #[case("198.18.0.1")] // benchmarking
    #[case("198.51.100.1")]
    #[case("203.0.113.1")]
    #[case("224.0.0.1")] // multicast
    #[case("240.0.0.1")] // reserved
    #[case("255.255.255.255")] // broadcast
    // IPv6 special-purpose ranges
    #[case("::")]
    #[case("::1")]
    #[case("fe80::1")] // link-local
    #[case("fc00::1")] // unique local
    #[case("fdaa:0:1:a7b::2")] // platform-internal IPv6
    #[case("ff02::1")] // multicast
    #[case("2001:db8::1")] // documentation
    #[case("100::1")] // discard-only
    // IPv4 embedded in IPv6 must be judged by the embedded address
    #[case("::ffff:127.0.0.1")] // IPv4-mapped
    #[case("::ffff:10.1.2.3")]
    #[case("64:ff9b::a00:1")] // NAT64 of 10.0.0.1
    fn non_public_addresses_are_refused(#[case] ip: &str) {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(!is_public(ip), "{ip} should not be public");
        assert!(!AddressPolicy::PUBLIC_ONLY.permits(ip));
        assert!(AddressPolicy::ALLOW_ALL.permits(ip));
    }

    #[rstest]
    #[case("::ffff:1.1.1.1")]
    #[case("64:ff9b::101:101")] // NAT64 of 1.1.1.1
    fn embedded_public_ipv4_is_public(#[case] ip: &str) {
        assert!(is_public(ip.parse().unwrap()));
    }

    #[test]
    fn default_policy_is_public_only() {
        assert_eq!(AddressPolicy::default(), AddressPolicy::PUBLIC_ONLY);
    }
}
