//! IPv4/IPv6 address range matching (CIDR, netmask, explicit ranges, single addresses),
//! accepting the formats of NetTools' `IPAddressRange.Parse` used by slskd.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

/// An inclusive range of addresses of one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpRange {
    V4 { start: u32, end: u32 },
    V6 { start: u128, end: u128 },
}

impl IpRange {
    /// Parses `addr`, `addr/prefix`, `v4addr/netmask`, `start-end` or `a.b.c.d-e`.
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();

        if input.is_empty() {
            return Err("empty address range".into());
        }

        if input.to_ascii_lowercase().starts_with("::ffff") {
            return Err("IPv4 mapped IPv6 addresses are not allowed".into());
        }

        if let Some((addr, suffix)) = input.split_once('/') {
            let addr = parse_addr(addr)?;
            let suffix = suffix.trim();

            return match addr {
                IpAddr::V4(v4) => {
                    let prefix = match suffix.parse::<u32>() {
                        Ok(prefix) if prefix <= 32 => prefix,
                        Ok(_) => return Err(format!("invalid IPv4 prefix length '{suffix}'")),
                        Err(_) => mask_prefix(suffix)?,
                    };
                    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
                    let start = u32::from(v4) & mask;
                    Ok(Self::V4 {
                        start,
                        end: start | !mask,
                    })
                }
                IpAddr::V6(v6) => {
                    let prefix = match suffix.parse::<u32>() {
                        Ok(prefix) if prefix <= 128 => prefix,
                        _ => return Err(format!("invalid IPv6 prefix length '{suffix}'")),
                    };
                    let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
                    let start = u128::from(v6) & mask;
                    Ok(Self::V6 {
                        start,
                        end: start | !mask,
                    })
                }
            };
        }

        if let Some((first, last)) = input.split_once('-') {
            let first = parse_addr(first)?;
            let last = last.trim();

            return match (first, last.parse::<IpAddr>()) {
                (IpAddr::V4(a), Ok(IpAddr::V4(b))) => ordered_v4(u32::from(a), u32::from(b)),
                (IpAddr::V6(a), Ok(IpAddr::V6(b))) => ordered_v6(u128::from(a), u128::from(b)),
                (IpAddr::V4(a), Err(_)) => {
                    let octet: u8 = last
                        .parse()
                        .map_err(|_| format!("invalid range end '{last}'"))?;
                    let [x, y, z, _] = a.octets();
                    ordered_v4(u32::from(a), u32::from_be_bytes([x, y, z, octet]))
                }
                _ => Err(format!("address family mismatch in range '{input}'")),
            };
        }

        Ok(match parse_addr(input)? {
            IpAddr::V4(v4) => {
                let v = u32::from(v4);
                Self::V4 { start: v, end: v }
            }
            IpAddr::V6(v6) => {
                let v = u128::from(v6);
                Self::V6 { start: v, end: v }
            }
        })
    }

    /// True when the range covers its whole address family (e.g. `0.0.0.0/0`, `::/0`).
    pub fn is_full(&self) -> bool {
        matches!(
            self,
            Self::V4 {
                start: 0,
                end: u32::MAX
            } | Self::V6 {
                start: 0,
                end: u128::MAX
            }
        )
    }

    /// True when `ip` lies in the range. IPv4-mapped IPv6 callers are matched as IPv4.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip.to_canonical()) {
            (Self::V4 { start, end }, IpAddr::V4(v4)) => (*start..=*end).contains(&u32::from(v4)),
            (Self::V6 { start, end }, IpAddr::V6(v6)) => (*start..=*end).contains(&u128::from(v6)),
            _ => false,
        }
    }
}

impl FromStr for IpRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for IpRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::V4 { start, end } => {
                write!(
                    f,
                    "{}-{}",
                    std::net::Ipv4Addr::from(start),
                    std::net::Ipv4Addr::from(end)
                )
            }
            Self::V6 { start, end } => {
                write!(
                    f,
                    "{}-{}",
                    std::net::Ipv6Addr::from(start),
                    std::net::Ipv6Addr::from(end)
                )
            }
        }
    }
}

/// Parses a comma separated list of ranges (slskd's `cidr` option format).
pub fn parse_ranges(list: &str) -> Result<Vec<IpRange>, String> {
    list.split(',')
        .map(|cidr| {
            IpRange::parse(cidr).map_err(|e| format!("CIDR {} is invalid: {e}", cidr.trim()))
        })
        .collect()
}

fn parse_addr(s: &str) -> Result<IpAddr, String> {
    let s = s.trim();
    s.parse().map_err(|_| format!("invalid IP address '{s}'"))
}

fn mask_prefix(mask: &str) -> Result<u32, String> {
    let m = u32::from(
        mask.parse::<std::net::Ipv4Addr>()
            .map_err(|_| format!("invalid netmask '{mask}'"))?,
    );

    if m.leading_ones() + m.trailing_zeros() != 32 {
        return Err(format!("netmask '{mask}' is not contiguous"));
    }

    Ok(m.leading_ones())
}

fn ordered_v4(start: u32, end: u32) -> Result<IpRange, String> {
    if start > end {
        return Err("range start is greater than range end".into());
    }
    Ok(IpRange::V4 { start, end })
}

fn ordered_v6(start: u128, end: u128) -> Result<IpRange, String> {
    if start > end {
        return Err("range start is greater than range end".into());
    }
    Ok(IpRange::V6 { start, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_v4() {
        let range = IpRange::parse("192.168.1.0/24").unwrap();
        assert!(range.contains(ip("192.168.1.0")));
        assert!(range.contains(ip("192.168.1.255")));
        assert!(!range.contains(ip("192.168.2.1")));
        assert!(!range.contains(ip("::1")));

        let host = IpRange::parse("10.1.2.3/32").unwrap();
        assert!(host.contains(ip("10.1.2.3")));
        assert!(!host.contains(ip("10.1.2.4")));

        let unaligned = IpRange::parse("10.1.2.3/8").unwrap();
        assert!(unaligned.contains(ip("10.200.0.1")));
    }

    #[test]
    fn everything() {
        let all4 = IpRange::parse("0.0.0.0/0").unwrap();
        let all6 = IpRange::parse("::/0").unwrap();
        assert!(all4.contains(ip("255.255.255.255")));
        assert!(all4.contains(ip("1.2.3.4")));
        assert!(!all4.contains(ip("fe80::1")));
        assert!(all6.contains(ip("fe80::1")));
        assert!(all6.contains(ip("::")));
    }

    #[test]
    fn cidr_v6() {
        let range = IpRange::parse("fe80::/10").unwrap();
        assert!(range.contains(ip("fe80::1")));
        assert!(range.contains(ip("febf:ffff::1")));
        assert!(!range.contains(ip("fec0::1")));
    }

    #[test]
    fn mapped_callers_match_v4() {
        let range = IpRange::parse("127.0.0.0/8").unwrap();
        assert!(range.contains(ip("::ffff:127.0.0.1")));
        assert!(IpRange::parse("::ffff:127.0.0.1").is_err());
    }

    #[test]
    fn netmask_and_ranges() {
        let masked = IpRange::parse("192.168.0.0/255.255.0.0").unwrap();
        assert_eq!(masked, IpRange::parse("192.168.0.0/16").unwrap());
        assert!(IpRange::parse("192.168.0.0/255.0.255.0").is_err());

        let range = IpRange::parse("10.0.0.5 - 10.0.0.9").unwrap();
        assert!(range.contains(ip("10.0.0.5")));
        assert!(range.contains(ip("10.0.0.9")));
        assert!(!range.contains(ip("10.0.0.10")));

        let short = IpRange::parse("10.0.0.5-20").unwrap();
        assert!(short.contains(ip("10.0.0.20")));
        assert!(!short.contains(ip("10.0.0.21")));

        assert!(IpRange::parse("10.0.0.9-10.0.0.1").is_err());
        assert!(IpRange::parse("10.0.0.1-::1").is_err());
    }

    #[test]
    fn single_and_invalid() {
        let single = IpRange::parse(" 1.2.3.4 ").unwrap();
        assert!(single.contains(ip("1.2.3.4")));
        assert!(!single.contains(ip("1.2.3.5")));

        assert!(IpRange::parse("").is_err());
        assert!(IpRange::parse("garbage").is_err());
        assert!(IpRange::parse("1.2.3.4/33").is_err());
        assert!(IpRange::parse("::/129").is_err());
    }

    #[test]
    fn lists() {
        let ranges = parse_ranges("0.0.0.0/0,::/0").unwrap();
        assert_eq!(ranges.len(), 2);
        assert!(parse_ranges("10.0.0.0/8, bogus").is_err());
    }
}
