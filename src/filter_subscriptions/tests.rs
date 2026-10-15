use super::*;

#[test]
fn fingerprint_binds_canonical_url_format_and_parser_version() {
    let a = Source::new("https://EXAMPLE.COM:443/a/../list", Format::DomainList).unwrap();
    let b = Source::new("https://example.com/list", Format::DomainList).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_ne!(
        a.fingerprint,
        Source::new("https://example.com/list", Format::HostsBlocklist)
            .unwrap()
            .fingerprint
    );
    assert_ne!(
        a.fingerprint,
        Source::new("https://example.com/other", Format::DomainList)
            .unwrap()
            .fingerprint
    );
    assert!(Source::new("http://example.com/list", Format::DomainList).is_err());
}
