//! Unicast DNS-SD (feature `dns-sd`): service discovery through an ordinary
//! DNS resolver (RFC 6763 over RFC 1035), for networks where multicast does
//! not reach, such as a Kubernetes cluster.
//!
//! Empty in this release: the feature and module are declared so that the
//! resolver (queries built with [`encode_query_for`](super::wire::encode_query_for)
//! with recursion desired, answers decoded with
//! [`Message::parse`](super::wire::Message::parse)) can be added without
//! changing the feature set. It will implement
//! [`Browse`](crate::discovery::Browse).
