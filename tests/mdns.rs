//! The discovery conformance cases against the mDNS backend (feature
//! `mdns`): an `MdnsAnnouncer` announcing to an `MdnsSocket` browser.
//!
//! Network conduct: both sockets are bound on 127.0.0.1 and aimed at each
//! other with an explicit destination, so nothing joins a multicast group
//! or sends to 224.0.0.251; the browser's query, the announcer's probes,
//! announcement and goodbye all travel over loopback unicast.
//!
//! The browser is a one-shot querier on an ephemeral port, but with an
//! explicit destination it accepts unsolicited datagrams from the
//! announcer, as a group member on port 5353 would.
#![cfg(feature = "mdns")]

mod conformance;

use conformance::{DiscoveryFixture, discovery};
use middleware::mdns::announce::MdnsAnnouncer;
use middleware::mdns::socket::MdnsSocket;

struct Mdns;

impl DiscoveryFixture for Mdns {
    type Announcer = MdnsAnnouncer;
    type Browser = MdnsSocket;

    async fn connect(&mut self) -> (MdnsAnnouncer, MdnsSocket) {
        let announcer_socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let browser = MdnsSocket::builder()
            .bind("127.0.0.1:0".parse().expect("addr"))
            .destination(announcer_socket.local_addr().expect("addr"))
            .build()
            .await
            .expect("browser");
        let announcer = MdnsAnnouncer::builder()
            .destination(browser.local_addr().expect("addr"))
            .host("conformance.local")
            .build_on(announcer_socket)
            .expect("announcer");
        (announcer, browser)
    }
}

#[tokio::test]
async fn l_announce_then_withdraw() {
    discovery::announce_then_withdraw(&mut Mdns).await;
}

#[tokio::test]
async fn l_goodbye_removes_record_at_once() {
    discovery::goodbye_removes_record_at_once(&mut Mdns).await;
}

#[test]
fn l_directory_expiry() {
    discovery::directory_expiry();
}
