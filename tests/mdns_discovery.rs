//! Advertises fake printers over real mDNS and checks that discovery finds
//! them, and forgets them when they're withdrawn. Needs a network interface
//! with multicast (loopback alone isn't used by mDNS).

mod support;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use inkdrop::discovery::{self, Printer, Registry};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use support::{Config, FakePrinter};
use tokio::sync::watch;

const WAIT: Duration = Duration::from_secs(20);

fn advertise(daemon: &ServiceDaemon, name: &str, port: u16, txt: &[(&str, &str)]) -> String {
    let host = format!("{}.local.", name.replace(' ', "-"));
    let service = ServiceInfo::new("_ipp._tcp.local.", name, &host, "", port, txt).unwrap().enable_addr_auto();
    let fullname = service.get_fullname().to_owned();
    daemon.register(service).unwrap();
    fullname
}

async fn wait_until(registry: &Registry, what: &str, condition: impl FnMut(&HashMap<String, Printer>) -> bool) {
    let mut receiver = registry.subscribe();
    tokio::time::timeout(WAIT, receiver.wait_for(condition))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting until {what}"))
        .unwrap();
}

fn named<'a>(printers: &'a HashMap<String, Printer>, name: &str) -> Option<&'a Printer> {
    printers.values().find(|p| p.name == name)
}

#[tokio::test(flavor = "multi_thread")]
async fn discovers_advertised_printers_and_forgets_withdrawn_ones() {
    support::init_tracing();
    // Discovery connects to the address mDNS advertises, so listen on all.
    let fake = FakePrinter::start_on(IpAddr::V4(Ipv4Addr::UNSPECIFIED), Config::raster_only(300, &["srgb_8"])).await;
    let pid = std::process::id();
    let listed = format!("inkdrop listed {pid}");
    let probed = format!("inkdrop probed {pid}");

    let registry: Registry = watch::channel(HashMap::new()).0;
    discovery::spawn(registry.clone());

    let daemon = ServiceDaemon::new().unwrap();
    let listed_fullname = advertise(&daemon, &listed, fake.port(), &[
        ("rp", "ipp/print"),
        ("ty", "Listed Model"),
        ("pdl", "application/pdf,image/urf"),
    ]);
    // No "pdl": discovery has to ask the (fake) printer what it takes.
    advertise(&daemon, &probed, fake.port(), &[("rp", "ipp/print")]);

    wait_until(&registry, "both printers are discovered", |printers| {
        named(printers, &listed).is_some() && named(printers, &probed).is_some()
    })
    .await;
    {
        let printers = registry.borrow();
        let listed = named(&printers, &listed).unwrap();
        assert_eq!(listed.formats, ["PDF", "URF"]);
        assert_eq!(listed.model.as_deref(), Some("Listed Model"));
        assert_eq!(listed.uri.port_u16(), Some(fake.port()));
        assert_eq!(listed.uri.path(), "/ipp/print");
        assert_eq!(named(&printers, &probed).unwrap().formats, ["PWG-Raster"]);
    }

    daemon.unregister(&listed_fullname).unwrap();
    wait_until(&registry, "the withdrawn printer is forgotten", |printers| named(printers, &listed).is_none()).await;
    assert!(named(&registry.borrow(), &probed).is_some());

    let _ = daemon.shutdown();
}
